//! Home Assistant REST switch backend (ureq, already a dep): POST
//! `/api/services/switch/turn_on|turn_off` with a long-lived Bearer token,
//! status from `GET /api/states/{entity}` (`state == "on"`). Config keys
//! shared with the Python backend: `base_url`, `token`, `entity_id`.

use super::super::actuators::{Plug, PlugError};
use crate::config::PlugCfg;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(4))
        .build()
}

pub struct HaPlug {
    name: String,
    base: String,
    token: String,
    entity: String,
}

impl HaPlug {
    /// All three optional fields are required for this backend; their
    /// absence is a skip (Ok(None)), not an error.
    pub fn from_config(cfg: &PlugCfg) -> Result<Option<HaPlug>, String> {
        match (
            cfg.base_url.clone(),
            cfg.token.clone(),
            cfg.entity_id.clone(),
        ) {
            (Some(base), Some(token), Some(entity)) if !base.is_empty() => Ok(Some(HaPlug {
                name: cfg.name.clone(),
                base: base.trim_matches('/').to_string(),
                token,
                entity,
            })),
            _ => Ok(None),
        }
    }

    fn service(&self, service: &str) -> Result<(), PlugError> {
        let url = format!("{}/api/services/switch/{}", self.base, service);
        let resp = agent()
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(ureq::json!({"entity_id": self.entity}));
        match resp {
            Ok(_) | Err(ureq::Error::Status(_, _)) => Ok(()),
            Err(e) => Err(PlugError::Failed(format!("ha {}: {e}", self.name))),
        }
    }
}

impl Plug for HaPlug {
    fn name(&self) -> &str {
        &self.name
    }

    fn on(&mut self) -> Result<(), PlugError> {
        self.service("turn_on")
    }

    fn off(&mut self) -> Result<(), PlugError> {
        self.service("turn_off")
    }

    /// `state == "on"` from the states API; None = unreachable/unparseable.
    fn status(&mut self) -> Option<bool> {
        let url = format!("{}/api/states/{}", self.base, self.entity);
        let body = agent()
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
            .ok()?
            .into_string()
            .ok()?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        Some(v.get("state")?.as_str()? == "on")
    }

    /// Entity attributes `power` or `current_power_w` (either naming);
    /// None when the entity exposes no wattage.
    fn power(&mut self) -> Option<f64> {
        let url = format!("{}/api/states/{}", self.base, self.entity);
        let body = agent()
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
            .ok()?
            .into_string()
            .ok()?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        let attrs = v.get("attributes")?;
        for k in ["power", "current_power_w"] {
            if let Some(w) = attrs.get(k).and_then(|x| x.as_f64()).filter(|w| *w >= 0.0) {
                return Some(w);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_routing() {
        let base = PlugCfg {
            backend: "ha".into(),
            name: "blower".into(),
            host: String::new(),
            gen: 1,
            channel: 0,
            id: String::new(),
            key: None,
            address: None,
            version: 3.3,
            base_url: None,
            token: None,
            entity_id: None,
        };
        assert!(HaPlug::from_config(&base).unwrap().is_none());
        let full = PlugCfg {
            base_url: Some("http://ha.lan:8123/".into()),
            token: Some("tok".into()),
            entity_id: Some("switch.blower".into()),
            ..base
        };
        let p = HaPlug::from_config(&full).unwrap().unwrap();
        assert_eq!(p.name(), "blower");
        assert_eq!(p.base, "http://ha.lan:8123", "trailing slash trimmed");
    }
}
