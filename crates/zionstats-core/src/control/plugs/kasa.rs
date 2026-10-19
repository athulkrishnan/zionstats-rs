//! Kasa (TP-Link) plug backend: raw TCP :9999 with the IOT autokey XOR
//! codec. Wire format verified against python-kasa 0.11.0.1
//! (`kasa/transports/xortransport.py`): requests are
//! `[len u32 BE][json XOR autokey(171)]` where the key starts at 171 and
//! advances with the CIPHERTEXT byte both ways; responses use the same
//! codec. Commands: `system.get_sysinfo` (relay_state),
//! `system.set_relay_state`, `emeter.get_realtime` (power metering lands
//! with the trait change).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use super::super::actuators::{Plug, PlugError};
use crate::config::PlugCfg;

pub const PORT: u16 = 9999;
const KEY0: u8 = 171;

/// Autokey XOR both ways (python-kasa-verified): encrypt advances the key
/// with the ciphertext byte, decrypt with the ciphertext byte.
pub fn xor_encode(plain: &[u8]) -> Vec<u8> {
    let mut key = KEY0;
    plain
        .iter()
        .map(|&b| {
            key ^= b;
            key
        })
        .collect()
}

pub fn xor_decode(cipher: &[u8]) -> Vec<u8> {
    let mut key = KEY0;
    cipher
        .iter()
        .map(|&c| {
            let p = key ^ c;
            key = c;
            p
        })
        .collect()
}

/// Frame one command: `[len u32 BE][xored json]`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&xor_encode(payload));
    out
}

/// Read one framed response from the stream; None on truncated/bad frames.
pub fn read_frame(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > 1 << 20 {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(xor_decode(&body))
}

pub struct KasaPlug {
    name: String,
    host: String,
    port: u16,
}

impl KasaPlug {
    pub fn from_config(cfg: &PlugCfg) -> Result<Option<KasaPlug>, String> {
        if cfg.host.is_empty() {
            return Err(format!("plug {:?}: missing host", cfg.name));
        }
        Ok(Some(KasaPlug {
            name: cfg.name.clone(),
            host: cfg.host.trim_matches('/').to_string(),
            port: PORT,
        }))
    }

    /// Send one JSON command and read the JSON response; None on any
    /// transport/parse failure (caller decides policy).
    fn request(&self, cmd: serde_json::Value) -> Option<serde_json::Value> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
        let body = serde_json::to_vec(&cmd).ok()?;
        stream.write_all(&frame(&body)).ok()?;
        stream.flush().ok()?;
        let plain = read_frame(&mut stream)?;
        serde_json::from_slice(&plain).ok()
    }

    fn set_relay(&mut self, on: bool) -> Result<(), PlugError> {
        let cmd = serde_json::json!({"system": {"set_relay_state": {"state": if on {1} else {0}}}});
        self.request(cmd)
            .map(|_| ())
            .ok_or_else(|| PlugError::Failed(format!("kasa {}: no/bad response", self.name)))
    }

    /// Extract the on/off state from a get_sysinfo response.
    fn parse_sysinfo_state(body: &serde_json::Value) -> Option<bool> {
        body.get("system")?
            .get("get_sysinfo")?
            .get("relay_state")
            .and_then(|v| v.as_i64())
            .map(|s| s == 1)
    }
}

impl Plug for KasaPlug {
    fn name(&self) -> &str {
        &self.name
    }

    fn on(&mut self) -> Result<(), PlugError> {
        self.set_relay(true)
    }

    fn off(&mut self) -> Result<(), PlugError> {
        self.set_relay(false)
    }

    fn status(&mut self) -> Option<bool> {
        let cmd = serde_json::json!({"system": {"get_sysinfo": {}}});
        Self::parse_sysinfo_state(&self.request(cmd)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn xor_codec_matches_reference() {
        // python-kasa XorEncryption: key starts 171, advances with the
        // ciphertext byte (encrypt) / ciphertext byte (decrypt).
        let plain = b"hello kasa";
        let enc = xor_encode(plain);
        assert_eq!(enc.len(), plain.len());
        // first byte: 0x68 ^ 171 = 0xD3
        assert_eq!(enc[0], 0x68u8 ^ 171u8);
        assert_eq!(xor_decode(&enc), plain);
        // byte-independence: a different plaintext yields a different stream
        assert_ne!(xor_encode(b"HELLO KASA"), enc);
    }

    #[test]
    fn frame_layout() {
        let f = frame(b"{}");
        assert_eq!(&f[..4], &[0, 0, 0, 2]);
        assert_eq!(f.len(), 6);
    }

    /// In-process mock server: asserts the received command decodes to the
    /// expected shape and replies with a scripted JSON body (encoded with
    /// the same codec -- round-trip proof).
    fn serve_one(
        respond: impl FnOnce(serde_json::Value) -> serde_json::Value + Send + 'static,
    ) -> (String, u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let plain = read_frame(&mut stream).unwrap();
            let cmd: serde_json::Value = serde_json::from_slice(&plain).unwrap();
            let resp = respond(cmd);
            stream
                .write_all(&frame(serde_json::to_vec(&resp).unwrap().as_slice()))
                .unwrap();
        });
        ("127.0.0.1".to_string(), port, handle)
    }

    #[test]
    fn status_round_trip_over_mock() {
        let (host, port, h) = serve_one(|cmd| {
            assert_eq!(cmd["system"]["get_sysinfo"], serde_json::json!({}));
            serde_json::json!({"system": {"get_sysinfo": {"relay_state": 1}}})
        });
        let mut plug = KasaPlug {
            name: "k".into(),
            host,
            port,
        };
        assert_eq!(plug.status(), Some(true));
        h.join().unwrap();
    }

    #[test]
    fn unreachable_is_none_not_panic() {
        let mut plug = KasaPlug {
            name: "k".into(),
            host: "127.0.0.1".into(),
            port: 1, // nothing listens here
        };
        assert_eq!(plug.status(), None);
        assert!(plug.on().is_err());
    }
}
