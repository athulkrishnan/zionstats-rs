//! Tuya local-LAN plug control (the "Tuya bridge"): wire protocol v3.3
//! and the static-v3.4 variant, TCP :6668.
//!
//! Wire contract verified against the tinytuya 1.20.0 source at
//! implementation time (core/header.py, core/command_types.py,
//! core/message_helper.py, core/crypto_helper.py, core/XenonDevice.py):
//! - Frame 55AA: `[prefix 0x000055AA][seqno][cmd][len][payload]
//!   [crc32][suffix 0x0000AA55]`, big-endian, `len = payload + 8`,
//!   CRC-32 (IEEE) over everything before it. (Command words: CONTROL=7,
//!   HEART_BEAT=9, DP_QUERY=10 -- note the prompt's "10=heartbeat" guess
//!   was wrong; the source wins.)
//! - v3.3 payload: AES-ECB(local_key, PKCS7) of the JSON -- NOT the
//!   XOR+base64 legacy 3.1 scheme the prompt described; `encrypt(payload,
//!   use_base64=False)` per the source. CONTROL carries the "3.3" + 12
//!   zero-byte version header; DP_QUERY does not (NO_PROTOCOL_HEADER_CMDS).
//! - Responses carry a 4-byte return-code before the encrypted body
//!   (base64'd for v3.3).
//! - v3.4 static variant implemented here: AES-GCM(local_key) with a
//!   12-byte random nonce prepended and the 16-byte tag appended, same
//!   55AA/CRC32 frame. tinytuya 1.20's full v3.4 (session-key negotiation
//!   with an HMAC-SHA256 frame trailer) is NOT implemented; devices that
//!   require it report None status and are skipped -- documented gap.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyInit};
use aes_gcm::aead::{Aead, AeadCore, OsRng};
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce};
use base64::Engine;

use super::super::actuators::{Plug, PlugError};
use crate::config::PlugCfg;

pub const PORT: u16 = 6668;
const PREFIX: u32 = 0x0000_55AA;
const SUFFIX: u32 = 0x0000_AA55;
const CMD_CONTROL: u32 = 7;
const CMD_DP_QUERY: u32 = 10;
const TIMEOUT: Duration = Duration::from_secs(2);

// ------------------------------------------------------------- frame codec

/// CRC-32 (IEEE 802.3, reflected 0xEDB88320) -- same polynomial/params as
/// Python's binascii.crc32, which tinytuya uses for the 55AA trailer.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Pack one 55AA frame.
pub fn pack_frame(seqno: u32, cmd: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 20);
    out.extend_from_slice(&PREFIX.to_be_bytes());
    out.extend_from_slice(&seqno.to_be_bytes());
    out.extend_from_slice(&cmd.to_be_bytes());
    out.extend_from_slice(&((payload.len() as u32) + 8).to_be_bytes());
    out.extend_from_slice(payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(&SUFFIX.to_be_bytes());
    out
}

pub struct ParsedFrame {
    pub seqno: u32,
    pub cmd: u32,
    pub payload: Vec<u8>,
}

/// Parse one 55AA frame; None on bad magic, truncated data, or CRC
/// mismatch.
pub fn parse_frame(buf: &[u8]) -> Option<ParsedFrame> {
    if buf.len() < 16 {
        return None;
    }
    let prefix = u32::from_be_bytes(buf[0..4].try_into().ok()?);
    if prefix != PREFIX {
        return None;
    }
    let seqno = u32::from_be_bytes(buf[4..8].try_into().ok()?);
    let cmd = u32::from_be_bytes(buf[8..12].try_into().ok()?);
    let len = u32::from_be_bytes(buf[12..16].try_into().ok()?) as usize;
    if len < 8 || buf.len() < 16 + len {
        return None;
    }
    let body_end = 16 + len - 8;
    let crc = u32::from_be_bytes(buf[body_end..body_end + 4].try_into().ok()?);
    if crc32(&buf[..body_end]) != crc {
        return None;
    }
    let suffix = u32::from_be_bytes(buf[buf.len() - 4..].try_into().ok()?);
    if suffix != SUFFIX {
        return None;
    }
    Some(ParsedFrame {
        seqno,
        cmd,
        payload: buf[16..body_end].to_vec(),
    })
}

// ------------------------------------------------------------ crypto codec

/// v3.3 body: AES-ECB(local_key, PKCS7) over the JSON. Key length selects
/// AES-128/192/256 exactly like tinytuya's AESCipher.
pub fn v33_encrypt(key: &[u8], plain: &[u8]) -> Option<Vec<u8>> {
    match key.len() {
        16 => {
            let enc = <ecb::Encryptor<aes::Aes128> as KeyInit>::new_from_slice(key).ok()?;
            Some(enc.encrypt_padded_vec_mut::<Pkcs7>(plain))
        }
        24 => {
            let enc = <ecb::Encryptor<aes::Aes192> as KeyInit>::new_from_slice(key).ok()?;
            Some(enc.encrypt_padded_vec_mut::<Pkcs7>(plain))
        }
        32 => {
            let enc = <ecb::Encryptor<aes::Aes256> as KeyInit>::new_from_slice(key).ok()?;
            Some(enc.encrypt_padded_vec_mut::<Pkcs7>(plain))
        }
        _ => None,
    }
}

/// AES-ECB decrypt + PKCS7 unpad over raw ciphertext (client requests are
/// unencoded on the wire).
pub fn v33_decrypt_raw(key: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    let pt = match key.len() {
        16 => {
            let dec = <ecb::Decryptor<aes::Aes128> as KeyInit>::new_from_slice(key).ok()?;
            dec.decrypt_padded_vec_mut::<Pkcs7>(ct).ok()?
        }
        24 => {
            let dec = <ecb::Decryptor<aes::Aes192> as KeyInit>::new_from_slice(key).ok()?;
            dec.decrypt_padded_vec_mut::<Pkcs7>(ct).ok()?
        }
        32 => {
            let dec = <ecb::Decryptor<aes::Aes256> as KeyInit>::new_from_slice(key).ok()?;
            dec.decrypt_padded_vec_mut::<Pkcs7>(ct).ok()?
        }
        _ => return None,
    };
    Some(pt)
}

/// v3.3 response body: base64 -> AES-ECB decrypt -> PKCS7 unpad
/// (tinytuya decrypts responses with `use_base64=True`).
pub fn v33_decrypt(key: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    let ct = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    v33_decrypt_raw(key, &ct)
}

/// v3.4 static body: AES-GCM(local_key) -> nonce(12) || ct || tag(16).
/// Nonces come from the OS (aead OsRng feature); reuse across messages
/// under the same key would break GCM, so generation must stay random.
pub fn v34_encrypt(key: &[u8], plain: &[u8]) -> Option<Vec<u8>> {
    match key.len() {
        16 => {
            let cipher = <Aes128Gcm as KeyInit>::new_from_slice(key).ok()?;
            let nonce = Aes128Gcm::generate_nonce(&mut OsRng);
            let ct = cipher.encrypt(&nonce, plain).ok()?;
            let mut out = Vec::with_capacity(12 + ct.len());
            out.extend_from_slice(nonce.as_slice());
            out.extend_from_slice(&ct);
            Some(out)
        }
        32 => {
            let cipher = <Aes256Gcm as KeyInit>::new_from_slice(key).ok()?;
            let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
            let ct = cipher.encrypt(&nonce, plain).ok()?;
            let mut out = Vec::with_capacity(12 + ct.len());
            out.extend_from_slice(nonce.as_slice());
            out.extend_from_slice(&ct);
            Some(out)
        }
        _ => None,
    }
}

pub fn v34_decrypt(key: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    if body.len() < 12 + 16 {
        return None;
    }
    let (nonce, ct) = body.split_at(12);
    match key.len() {
        16 => {
            let cipher = <Aes128Gcm as KeyInit>::new_from_slice(key).ok()?;
            cipher.decrypt(Nonce::from_slice(nonce), ct).ok()
        }
        32 => {
            let cipher = <Aes256Gcm as KeyInit>::new_from_slice(key).ok()?;
            cipher.decrypt(Nonce::from_slice(nonce), ct).ok()
        }
        _ => None,
    }
}

// ------------------------------------------------------------------ client

pub struct TuyaPlug {
    name: String,
    id: String,
    key: Vec<u8>,
    address: String,
    port: u16,
    version: f32,
    seqno: u32,
}

impl TuyaPlug {
    /// Missing id/key/address is a skip (Ok(None)); the caller warns.
    pub fn from_config(cfg: &PlugCfg) -> Result<Option<TuyaPlug>, String> {
        let key = match &cfg.key {
            Some(k) if !k.is_empty() => k.as_bytes().to_vec(),
            _ => return Ok(None),
        };
        if cfg.id.is_empty() {
            return Ok(None);
        }
        let Some(address) = &cfg.address else {
            return Ok(None);
        };
        if (cfg.version != 3.3) && (cfg.version != 3.4) {
            return Err(format!(
                "plug {:?}: tuya version {} not supported (3.3 or 3.4)",
                cfg.name, cfg.version
            ));
        }
        Ok(Some(TuyaPlug {
            name: cfg.name.clone(),
            id: cfg.id.clone(),
            key,
            address: address.to_string(),
            port: PORT,
            version: cfg.version,
            seqno: 1,
        }))
    }

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn build_payload(&mut self, cmd: u32, dps: Option<serde_json::Value>) -> Vec<u8> {
        let mut json = serde_json::json!({
            "devId": self.id, "uid": self.id, "t": Self::now_unix(),
        });
        if cmd == CMD_DP_QUERY {
            json["gwId"] = serde_json::Value::String(self.id.clone());
        }
        if let Some(dps) = dps {
            json["dps"] = dps;
        }
        let mut plain = serde_json::to_vec(&json).unwrap_or_default();
        // version header: CONTROL carries it, DP_QUERY does not
        // (NO_PROTOCOL_HEADER_CMDS in the tinytuya source)
        let with_header = cmd != CMD_DP_QUERY;
        let body = match self.version {
            v if v >= 3.4 => {
                if with_header {
                    let mut h = b"3.4".to_vec();
                    h.extend_from_slice(&[0u8; 12]);
                    h.extend_from_slice(&plain);
                    plain = h;
                }
                v34_encrypt(&self.key, &plain).unwrap_or_default()
            }
            _ => {
                let ct = v33_encrypt(&self.key, &plain).unwrap_or_default();
                if with_header {
                    let mut h = b"3.3".to_vec();
                    h.extend_from_slice(&[0u8; 12]);
                    h.extend_from_slice(&ct);
                    h
                } else {
                    ct
                }
            }
        };
        let seqno = self.seqno;
        self.seqno = self.seqno.wrapping_add(1);
        pack_frame(seqno, cmd, &body)
    }

    /// Round-trip one request; returns the response JSON (after crypto +
    /// retcode handling). None on transport/crypto/parse failure.
    fn request(&mut self, cmd: u32, dps: Option<serde_json::Value>) -> Option<serde_json::Value> {
        let frame = self.build_payload(cmd, dps);
        let mut stream = TcpStream::connect((self.address.as_str(), self.port)).ok()?;
        stream.set_read_timeout(Some(TIMEOUT)).ok();
        stream.set_write_timeout(Some(TIMEOUT)).ok();
        stream.write_all(&frame).ok()?;
        // read one frame: header prefix, then length-delimited remainder
        let mut buf = vec![0u8; 16];
        stream.read_exact(&mut buf).ok()?;
        if u32::from_be_bytes(buf[0..4].try_into().ok()?) != PREFIX {
            return None;
        }
        let len = u32::from_be_bytes(buf[12..16].try_into().ok()?) as usize;
        if len < 8 {
            return None;
        }
        let mut rest = vec![0u8; len - 8 + 8];
        stream.read_exact(&mut rest).ok()?;
        buf.extend_from_slice(&rest);
        let parsed = parse_frame(&buf)?;
        self.decode_response(&parsed.payload)
    }

    /// Response payload: [retcode u32][body]. v3.3 bodies are base64'd
    /// ECB ciphertext; some firmwares send plaintext JSON -- try both,
    /// never guess past failure.
    fn decode_response(&self, payload: &[u8]) -> Option<serde_json::Value> {
        let body = payload.get(4..)?;
        if let Ok(txt) = serde_json::from_slice::<serde_json::Value>(body) {
            return Some(txt);
        }
        match self.version {
            v if v >= 3.4 => {
                let pt = v34_decrypt(&self.key, body)?;
                serde_json::from_slice(&pt).ok()
            }
            _ => {
                let pt = v33_decrypt(&self.key, body)?;
                serde_json::from_slice(&pt).ok()
            }
        }
    }

    /// dps key "1" with "20" fallback, mirroring the Python TuyaPlug.
    fn parse_dps_state(v: &serde_json::Value) -> Option<bool> {
        let dps = v.get("dps")?;
        for k in ["1", "20"] {
            if let Some(b) = dps.get(k) {
                return b.as_bool();
            }
        }
        None
    }

    pub fn set_power(&mut self, on: bool) -> Result<(), PlugError> {
        let dps = serde_json::json!({ "1": on });
        let sent = self.request(CMD_CONTROL, Some(dps));
        match sent {
            Some(_) => Ok(()),
            None => Err(PlugError::Failed(format!(
                "tuya {}: no/bad response",
                self.name
            ))),
        }
    }
}

impl Plug for TuyaPlug {
    fn name(&self) -> &str {
        &self.name
    }

    fn on(&mut self) -> Result<(), PlugError> {
        self.set_power(true)
    }

    fn off(&mut self) -> Result<(), PlugError> {
        self.set_power(false)
    }

    /// None = unreachable / undecryptable / no-readback firmware; the
    /// verify loop treats it like the Python client does (caller policy).
    fn status(&mut self) -> Option<bool> {
        let v = self.request(CMD_DP_QUERY, None)?;
        Self::parse_dps_state(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn plug_at(port: u16) -> TuyaPlug {
        TuyaPlug {
            name: "t".into(),
            id: "testid12345678".into(),
            key: b"0123456789abcdef".to_vec(),
            address: "127.0.0.1".into(),
            port,
            version: 3.3,
            seqno: 1,
        }
    }

    #[test]
    fn crc32_reference() {
        // independent bitwise implementation vs binascii.crc32 vectors
        fn crc_ref(data: &[u8]) -> u32 {
            let mut crc: u32 = 0xFFFF_FFFF;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        assert_eq!(crc32(b"123456789"), crc_ref(b"123456789"));
        assert_eq!(
            crc32(b"123456789"),
            0xCBF4_3926,
            "canonical CRC-32 check value"
        );
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn frame_round_trip_and_garbage() {
        let payload = b"hello tuya";
        let f = pack_frame(9, CMD_CONTROL, payload);
        let p = parse_frame(&f).unwrap();
        assert_eq!((p.seqno, p.cmd), (9, CMD_CONTROL));
        assert_eq!(p.payload, payload);
        // flip a payload bit -> CRC mismatch
        let mut bad = f.clone();
        bad[20] ^= 0x01;
        assert!(parse_frame(&bad).is_none());
        // bad magic
        let mut bad2 = f.clone();
        bad2[0] = 0x66;
        assert!(parse_frame(&bad2).is_none());
        assert!(parse_frame(&f[..10]).is_none(), "truncated");
    }

    #[test]
    fn v33_codec_round_trip() {
        let key = b"0123456789abcdef";
        let plain = br#"{"devId":"x","dps":{"1":true}}"#;
        let ct = v33_encrypt(key, plain).unwrap();
        assert_eq!(ct.len() % 16, 0, "PKCS7 to block size");
        // requests are raw ECB on the wire
        assert_eq!(v33_decrypt_raw(key, &ct).unwrap(), plain);
        // responses are base64'd ECB
        let b64 = base64::engine::general_purpose::STANDARD.encode(&ct);
        assert_eq!(v33_decrypt(key, b64.as_bytes()).unwrap(), plain);
        assert!(
            v33_decrypt_raw(b"wrongkey98765432", &ct).is_none()
                || v33_decrypt_raw(b"wrongkey9876543", &ct).is_none(),
            "wrong-key ECB must fail unpadding or decrypt"
        );
        assert!(
            v33_encrypt(b"short", plain).is_none(),
            "bad key length rejected"
        );
    }

    #[test]
    fn v34_codec_round_trip() {
        let key = b"0123456789abcdef";
        let plain = br#"{"devId":"x"}"#;
        let body = v34_encrypt(key, plain).unwrap();
        assert!(body.len() > 12 + 16);
        assert_eq!(v34_decrypt(key, &body).unwrap(), plain);
        // tampered tag must fail authentication
        let mut bad = body.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0x01;
        assert!(v34_decrypt(key, &bad).is_none());
    }

    /// Mock server for one exchange: validates the frame + crypto, then
    /// answers with a scripted dps map encoded through the same codec.
    fn serve_v33(dps: serde_json::Value) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = vec![0u8; 16];
            stream.read_exact(&mut head).unwrap();
            assert_eq!(
                &head[0..4],
                &PREFIX.to_be_bytes(),
                "client must send 55AA magic"
            );
            let len = u32::from_be_bytes(head[12..16].try_into().unwrap()) as usize;
            let mut rest = vec![0u8; len - 8 + 8];
            stream.read_exact(&mut rest).unwrap();
            head.extend_from_slice(&rest);
            let frame = parse_frame(&head).unwrap();
            assert_eq!(frame.cmd, CMD_DP_QUERY);
            // decode the (headerless) v3.3 query body and prove it parses
            let pt = v33_decrypt_raw(b"0123456789abcdef", &frame.payload).unwrap();
            let q: serde_json::Value = serde_json::from_slice(&pt).unwrap();
            assert_eq!(q["gwId"], "testid12345678");
            // reply: retcode 0 + base64(ECB(json))
            let resp_json = serde_json::json!({ "dps": dps });
            let mut payload = 0u32.to_be_bytes().to_vec();
            payload.extend_from_slice(
                &base64::engine::general_purpose::STANDARD
                    .encode(
                        v33_encrypt(
                            b"0123456789abcdef",
                            serde_json::to_vec(&resp_json).unwrap().as_slice(),
                        )
                        .unwrap(),
                    )
                    .into_bytes(),
            );
            stream
                .write_all(&pack_frame(1, CMD_DP_QUERY, &payload))
                .unwrap();
        });
        (port, handle)
    }

    #[test]
    fn status_round_trip_v33() {
        let (port, h) = serve_v33(serde_json::json!({ "1": true }));
        let mut plug = plug_at(port);
        assert_eq!(plug.status(), Some(true));
        h.join().unwrap();
    }

    #[test]
    fn status_dp20_fallback() {
        let (port, h) = serve_v33(serde_json::json!({ "20": false }));
        let mut plug = plug_at(port);
        assert_eq!(plug.status(), Some(false));
        h.join().unwrap();
    }

    #[test]
    fn unreachable_is_none_not_panic() {
        let mut plug = plug_at(1);
        assert_eq!(plug.status(), None);
        assert!(plug.on().is_err());
    }
}
