//! HTTP plug backends: Shelly (Gen1 REST + Gen2 RPC) and Tasmota. Plain
//! HTTP only (LAN, no TLS) — same posture as the Python backends. Wire
//! shapes ported from `zionstats/plugs.py`; the Tasmota status key lookup
//! is case-insensitive (real devices reply `POWER`, Python read `Power` —
//! documented improvement).

use super::super::actuators::{Plug, PlugError};
use crate::config::PlugCfg;

fn http_get(url: &str) -> Result<String, PlugError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(4))
        .build();
    match agent.get(url).call() {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| PlugError::Failed(e.to_string())),
        Err(ureq::Error::Status(code, _)) => Err(PlugError::Failed(format!("http {code}"))),
        Err(e) => Err(PlugError::Failed(e.to_string())),
    }
}

pub struct HttpPlug {
    name: String,
    kind: HttpKind,
    host: String,
    channel: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum HttpKind {
    ShellyGen1,
    ShellyGen2,
    Tasmota,
}

impl HttpPlug {
    /// Build from config; only the HTTP backends route here (the
    /// `plugs::from_config` factory dispatches first).
    pub fn from_config(cfg: &PlugCfg) -> Result<Option<HttpPlug>, String> {
        let kind = match cfg.backend.as_str() {
            "shelly" => {
                if cfg.gen >= 2 {
                    HttpKind::ShellyGen2
                } else {
                    HttpKind::ShellyGen1
                }
            }
            "tasmota" => HttpKind::Tasmota,
            other => return Err(format!("unknown plug backend {other:?}")),
        };
        if cfg.host.is_empty() {
            return Err(format!("plug {:?}: missing host", cfg.name));
        }
        Ok(Some(HttpPlug {
            name: cfg.name.clone(),
            kind,
            host: cfg.host.trim_matches('/').to_string(),
            channel: cfg.channel,
        }))
    }

    fn set_url(&self, on: bool) -> String {
        match self.kind {
            HttpKind::ShellyGen1 => {
                format!(
                    "http://{}/relay/{}?turn={}",
                    self.host,
                    self.channel,
                    if on { "on" } else { "off" }
                )
            }
            HttpKind::ShellyGen2 => {
                format!(
                    "http://{}/rpc/Switch.Set?id={}&on={}",
                    self.host, self.channel, on
                )
            }
            HttpKind::Tasmota => {
                format!(
                    "http://{}/cm?cmnd=Power%20{}",
                    self.host,
                    if on { "ON" } else { "OFF" }
                )
            }
        }
    }

    fn status_url(&self) -> String {
        match self.kind {
            HttpKind::ShellyGen1 => {
                format!("http://{}/relay/{}", self.host, self.channel)
            }
            HttpKind::ShellyGen2 => {
                format!(
                    "http://{}/rpc/Switch.GetStatus?id={}",
                    self.host, self.channel
                )
            }
            HttpKind::Tasmota => {
                format!("http://{}/cm?cmnd=Power", self.host)
            }
        }
    }

    /// Extract the on/off state from a status payload. Case-insensitive
    /// key+value lookup (Gen1 `output`, Gen2 `output`, Tasmota `POWER`).
    fn parse_state(kind: HttpKind, body: &str) -> Option<bool> {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        match kind {
            HttpKind::ShellyGen1 | HttpKind::ShellyGen2 => v.get("output")?.as_bool(),
            HttpKind::Tasmota => {
                // accept POWER/Power/power keys and ON/OFF values
                for (k, val) in v.as_object()? {
                    if k.eq_ignore_ascii_case("power") {
                        return val.as_str().map(|s| s.eq_ignore_ascii_case("on"));
                    }
                }
                None
            }
        }
    }
}

impl Plug for HttpPlug {
    fn name(&self) -> &str {
        &self.name
    }

    fn on(&mut self) -> Result<(), PlugError> {
        http_get(&self.set_url(true)).map(|_| ())
    }

    fn off(&mut self) -> Result<(), PlugError> {
        http_get(&self.set_url(false)).map(|_| ())
    }

    /// None = unreachable or no readback support (verify-loop treats this
    /// as success — write-then-trust, Python parity).
    fn status(&mut self) -> Option<bool> {
        let body = http_get(&self.status_url()).ok()?;
        Self::parse_state(self.kind, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_match_python_shapes() {
        let shelly1 = HttpPlug {
            name: "b".into(),
            kind: HttpKind::ShellyGen1,
            host: "192.168.1.50".into(),
            channel: 0,
        };
        assert_eq!(shelly1.set_url(true), "http://192.168.1.50/relay/0?turn=on");
        assert_eq!(shelly1.status_url(), "http://192.168.1.50/relay/0");

        let shelly2 = HttpPlug {
            kind: HttpKind::ShellyGen2,
            host: "10.0.0.8".into(),
            channel: 1,
            name: shelly1.name.clone(),
        };
        assert_eq!(
            shelly2.set_url(false),
            "http://10.0.0.8/rpc/Switch.Set?id=1&on=false"
        );

        let tasmota = HttpPlug {
            kind: HttpKind::Tasmota,
            host: "plug.lan".into(),
            name: shelly1.name.clone(),
            channel: shelly1.channel,
        };
        assert_eq!(tasmota.set_url(true), "http://plug.lan/cm?cmnd=Power%20ON");
        assert_eq!(tasmota.status_url(), "http://plug.lan/cm?cmnd=Power");
    }

    #[test]
    fn state_parsing() {
        assert_eq!(
            HttpPlug::parse_state(HttpKind::ShellyGen1, r#"{"ison":false,"output":true}"#),
            Some(true)
        );
        assert_eq!(
            HttpPlug::parse_state(HttpKind::ShellyGen2, r#"{"output":false}"#),
            Some(false)
        );
        assert_eq!(
            HttpPlug::parse_state(HttpKind::Tasmota, r#"{"POWER":"ON"}"#),
            Some(true)
        );
        assert_eq!(
            HttpPlug::parse_state(HttpKind::Tasmota, r#"{"Power":"OFF"}"#),
            Some(false)
        );
        assert_eq!(HttpPlug::parse_state(HttpKind::Tasmota, "not json"), None);
    }

    #[test]
    fn backend_routing() {
        let cfg = PlugCfg {
            backend: "shelly".into(),
            name: "t".into(),
            host: "x".into(),
            gen: 3,
            channel: 0,
            id: String::new(),
            key: None,
            address: None,
            version: 3.3,
            base_url: None,
            token: None,
            entity_id: None,
        };
        assert!(HttpPlug::from_config(&cfg).unwrap().is_some());
        let bad = PlugCfg {
            backend: "nope".into(),
            ..cfg.clone()
        };
        assert!(HttpPlug::from_config(&bad).is_err());
        let nohost = PlugCfg {
            backend: "shelly".into(),
            host: String::new(),
            ..cfg
        };
        assert!(HttpPlug::from_config(&nohost).is_err());
    }
}
