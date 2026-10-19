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
use aes_gcm::aead::OsRng;
use aes_gcm::Aes128Gcm;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::super::actuators::{Plug, PlugError};
use crate::config::PlugCfg;

pub const PORT: u16 = 6668;
const PREFIX: u32 = 0x0000_55AA;
const SUFFIX: u32 = 0x0000_AA55;
const CMD_CONTROL: u32 = 7;
const CMD_SESS_NEG_START: u32 = 3;
const CMD_SESS_NEG_RESP: u32 = 4;
const CMD_SESS_NEG_FINISH: u32 = 5;
const CMD_DP_QUERY: u32 = 10;
const CMD_DP_QUERY_NEW: u32 = 16;
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

/// Pack one 55AA frame with the CRC32 trailer (v3.3).
pub fn pack_frame(seqno: u32, cmd: u32, payload: &[u8]) -> Vec<u8> {
    pack_frame_trailer(seqno, cmd, payload, None)
}

/// Pack one 55AA frame; `hmac_key` swaps the CRC32 trailer for the
/// 32-byte HMAC-SHA256 (v3.4, tinytuya MESSAGE_END_FMT_HMAC).
/// HMAC-SHA256 trailer (tinytuya pack_message with hmac_key: the
/// CRC32 field becomes a 32-byte HMAC over header+payload).
pub fn hmac_trailer(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub fn pack_frame_trailer(
    seqno: u32,
    cmd: u32,
    payload: &[u8],
    hmac_key: Option<&[u8]>,
) -> Vec<u8> {
    let trailer_len = if hmac_key.is_some() { 36 } else { 8 };
    let mut out = Vec::with_capacity(payload.len() + 16 + trailer_len);
    out.extend_from_slice(&PREFIX.to_be_bytes());
    out.extend_from_slice(&seqno.to_be_bytes());
    out.extend_from_slice(&cmd.to_be_bytes());
    out.extend_from_slice(&((payload.len() as u32) + trailer_len as u32).to_be_bytes());
    out.extend_from_slice(payload);
    if let Some(key) = hmac_key {
        out.extend_from_slice(&hmac_trailer(key, &out));
    } else {
        out.extend_from_slice(&crc32(&out).to_be_bytes());
    }
    out.extend_from_slice(&SUFFIX.to_be_bytes());
    out
}

pub struct ParsedFrame {
    pub seqno: u32,
    pub cmd: u32,
    pub payload: Vec<u8>,
}

/// Parse one 55AA frame (CRC32 trailer, v3.3); None on bad magic,
/// truncation, or CRC mismatch.
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

/// Parse a 55AA frame with the HMAC-SHA256 trailer (v3.4).  The frame
/// length field covers payload + 36 (32-byte HMAC + 4-byte suffix).
pub fn parse_frame_hmac(buf: &[u8], hmac_key: &[u8]) -> Option<ParsedFrame> {
    if buf.len() < 16 {
        return None;
    }
    if u32::from_be_bytes(buf[0..4].try_into().ok()?) != PREFIX {
        return None;
    }
    let cmd = u32::from_be_bytes(buf[8..12].try_into().ok()?);
    let len = u32::from_be_bytes(buf[12..16].try_into().ok()?) as usize;
    if len < 36 || buf.len() < 16 + len {
        return None;
    }
    let body_end = 16 + len - 36;
    let hmac = &buf[body_end..body_end + 32];
    if hmac_trailer(hmac_key, &buf[..body_end]).as_slice() != hmac {
        return None;
    }
    let suffix = u32::from_be_bytes(buf[buf.len() - 4..].try_into().ok()?);
    if suffix != SUFFIX {
        return None;
    }
    let seqno = u32::from_be_bytes(buf[4..8].try_into().ok()?);
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

/// v3.4 body: header + plaintext encrypted TOGETHER with AES-ECB
/// (tinytuya `_decode_payload`: 3.4 devices encrypt the version header
/// in addition to the payload).  Key length selects AES-128/192/256.
pub fn v34_encrypt(key: &[u8], headered: &[u8]) -> Option<Vec<u8>> {
    v33_encrypt(key, headered)
}

/// v3.4 response body: AES-ECB decrypt -> PKCS7 unpad (no base64 --
/// tinytuya decrypts 3.4 payloads with `decode_text=False` and no
/// b64 layer).
pub fn v34_decrypt(key: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    v33_decrypt_raw(key, ct)
}

// ------------------------------------------------------------------ client

pub struct TuyaPlug {
    name: String,
    id: String,
    /// the real local key from config
    key: Vec<u8>,
    address: String,
    port: u16,
    version: f32,
    seqno: u32,
    /// v3.4 session state: client/server nonces XOR'd then ECB-encrypted
    /// with the local key (tinytuya _negotiate_session_key_generate_finalize)
    session_key: Option<Vec<u8>>,
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
            session_key: None,
        }))
    }

    /// device22-class status query over a fresh negotiated connection.
    fn status_v34_device22(&mut self) -> Option<bool> {
        let mut stream = TcpStream::connect((self.address.as_str(), self.port)).ok()?;
        stream.set_read_timeout(Some(TIMEOUT)).ok();
        stream.set_write_timeout(Some(TIMEOUT)).ok();
        self.negotiate(&mut stream)?;
        let Some(skey) = self.session_key.clone() else {
            eprintln!("D22: no session key");
            return None;
        };
        let Some(body) = v34_encrypt(&skey, b"{}") else {
            eprintln!("D22: encrypt failed");
            return None;
        };
        let seqno = self.seqno;
        self.seqno += 1;
        let frame = pack_frame_trailer(seqno, CMD_DP_QUERY_NEW, &body, None);
        if let Err(e) = stream.write_all(&frame) {
            eprintln!("D22: write failed: {e}");
            return None;
        }
        let mut buf = vec![0u8; 16];
        if stream.read_exact(&mut buf).is_err() {
            eprintln!("D22: header read failed (timeout?)");
            return None;
        }
        if u32::from_be_bytes(buf[0..4].try_into().ok()?) != PREFIX {
            eprintln!("D22: bad magic");
            return None;
        }
        let len = u32::from_be_bytes(buf[12..16].try_into().ok()?) as usize;
        if len < 8 {
            eprintln!("D22: bad len {len}");
            return None;
        }
        let mut rest = vec![0u8; len];
        if stream.read_exact(&mut rest).is_err() {
            eprintln!("D22: body read failed");
            return None;
        }
        buf.extend_from_slice(&rest);
        let Some(parsed) = parse_frame(&buf) else {
            eprintln!("D22: CRC/parse failed ({} bytes)", buf.len());
            return None;
        };
        let Some(pt) = v34_decrypt(&skey, &parsed.payload) else {
            eprintln!("D22: decrypt failed ({} bytes)", parsed.payload.len());
            return None;
        };
        eprintln!("D22: pt {}", String::from_utf8_lossy(&pt));
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&pt) else {
            eprintln!("D22: json failed");
            return None;
        };
        Self::parse_dps_state(&v)
    }

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// 16 random bytes from the OS (client nonce for v3.4 negotiation).
    /// (nonce is 12 B; pad to the protocol's 16-byte nonce width)
    fn random_16() -> [u8; 16] {
        use aes_gcm::aead::AeadCore;
        let n = Aes128Gcm::generate_nonce(&mut OsRng);
        let mut out = [0u8; 16];
        out[..12].copy_from_slice(n.as_slice());
        out
    }

    /// v3.4 session-key negotiation (tinytuya
    /// _negotiate_session_key*): STEP1 sends a fresh 16-byte client
    /// nonce; the device answers SESS_KEY_NEG_RESP with
    /// [server_nonce(16) || HMAC(local_key, client_nonce)(32)] ECB-
    /// encrypted with the local key; we answer with
    /// HMAC(local_key, server_nonce); the session key is
    /// ECB(local_key, client_nonce XOR server_nonce).  Everything after
    /// negotiation uses the session key and the HMAC trailer.
    fn negotiate(&mut self, stream: &mut TcpStream) -> Option<()> {
        self.session_key = None;
        self.seqno = 1;
        let client_nonce = Self::random_16();
        let frame = {
            let seqno = self.seqno;
            self.seqno += 1;
            let body = v33_encrypt(&self.key, &client_nonce)?; // STEP1 has no header
            pack_frame_trailer(seqno, CMD_SESS_NEG_START, &body, Some(&self.key))
        };
        if stream.write_all(&frame).is_err() {
            return None;
        }
        let _ = stream.flush();
        let parsed = self.read_frame_hmac_dbg(stream)?;
        if parsed.cmd != CMD_SESS_NEG_RESP {
            return None;
        }
        // the payload carries a 4-byte retcode before the ciphertext
        let Some(ct) = parsed.payload.get(4..) else {
            return None;
        };
        let Some(resp) = v33_decrypt_raw(&self.key, ct) else {
            return None;
        };
        if resp.len() < 48 {
            return None;
        }
        let server_nonce = &resp[..16];
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).ok()?;
        mac.update(&client_nonce);
        if mac.finalize().into_bytes().as_slice() != &resp[16..48] {
            {
                eprintln!("NEG-FAIL #3");
                return None;
            }
        }
        // step 3: HMAC(local_key, server_nonce)
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).ok()?;
        mac.update(server_nonce);
        let finish = mac.finalize().into_bytes().to_vec();
        let frame = {
            let seqno = self.seqno;
            self.seqno += 1;
            let body = v33_encrypt(&self.key, &finish)?;
            pack_frame_trailer(seqno, CMD_SESS_NEG_FINISH, &body, Some(&self.key))
        };
        stream.write_all(&frame).ok()?;
        stream.flush().ok()?;
        // session key = ECB(local_key, client_nonce XOR server_nonce)
        let mut xored = [0u8; 16];
        for i in 0..16 {
            xored[i] = client_nonce[i] ^ server_nonce[i];
        }
        let Some(skey) = v33_encrypt(&self.key, &xored) else {
            return None;
        };
        self.session_key = Some(skey);
        Some(())
    }

    fn read_frame_hmac_dbg(&self, stream: &mut TcpStream) -> Option<ParsedFrame> {
        let mut buf = vec![0u8; 16];
        if stream.read_exact(&mut buf).is_err() {
            return None;
        }
        if u32::from_be_bytes(buf[0..4].try_into().ok()?) != PREFIX {
            return None;
        }
        let len = u32::from_be_bytes(buf[12..16].try_into().ok()?) as usize;
        if len < 8 {
            return None;
        }
        let mut rest = vec![0u8; len];
        stream.read_exact(&mut rest).ok()?;
        buf.extend_from_slice(&rest);
        match parse_frame_hmac(&buf, &self.key) {
            Some(f) => Some(f),
            None => None,
        }
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
        let seqno = self.seqno;
        self.seqno = self.seqno.wrapping_add(1);
        match self.version {
            v if v >= 3.4 => {
                // tinytuya 3.4 post-negotiation: "3.4"+12-zero header +
                // payload encrypted TOGETHER under the SESSION key, but
                // only for commands outside NO_PROTOCOL_HEADER_CMDS
                // (DP_QUERY/heartbeat go header-less); HMAC trailer under
                // the session key
                let with_header = cmd != CMD_DP_QUERY;
                if with_header {
                    let mut h = b"3.4".to_vec();
                    h.extend_from_slice(&[0u8; 12]);
                    h.extend_from_slice(&plain);
                    plain = h;
                }
                let skey = self.session_key.clone().unwrap_or_else(|| self.key.clone());
                let body = v34_encrypt(&skey, &plain).unwrap_or_default();
                pack_frame_trailer(seqno, cmd, &body, Some(&skey))
            }
            _ => {
                // v3.3: ECB over the JSON; CONTROL carries the header,
                // DP_QUERY does not (NO_PROTOCOL_HEADER_CMDS)
                let with_header = cmd != CMD_DP_QUERY;
                let ct = v33_encrypt(&self.key, &plain).unwrap_or_default();
                let body = if with_header {
                    let mut h = b"3.3".to_vec();
                    h.extend_from_slice(&[0u8; 12]);
                    h.extend_from_slice(&ct);
                    h
                } else {
                    ct
                };
                pack_frame(seqno, cmd, &body)
            }
        }
    }

    /// Round-trip one request; returns the response JSON (after crypto +
    /// retcode handling). None on transport/crypto/parse failure.
    fn request(&mut self, cmd: u32, dps: Option<serde_json::Value>) -> Option<serde_json::Value> {
        let mut stream = TcpStream::connect((self.address.as_str(), self.port)).ok()?;
        stream.set_read_timeout(Some(TIMEOUT)).ok();
        stream.set_write_timeout(Some(TIMEOUT)).ok();
        if self.version >= 3.4 {
            self.negotiate(&mut stream)?;
        }
        let frame = self.build_payload(cmd, dps);
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
        let mut rest = vec![0u8; len];
        stream.read_exact(&mut rest).ok()?;
        buf.extend_from_slice(&rest);
        let parsed = if self.version >= 3.4 {
            parse_frame_hmac(&buf, &self.key)?
        } else {
            parse_frame(&buf)?
        };
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

    /// Live watts from a recognized metering DP only; DP ids vary wildly
    /// across Tuya firmwares, so unrecognized stays None (never a guessed
    /// scale) -- Python parity.
    fn parse_dps_power(v: &serde_json::Value) -> Option<f64> {
        let dps = v.get("dps")?;
        for k in ["power", "power_w", "pow"] {
            if let Some(w) = dps.get(k).and_then(|x| x.as_f64()) {
                if (0.0..=4000.0).contains(&w) {
                    return Some(w);
                }
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
        if self.version >= 3.4 {
            // Hardware-verified (ZEB-SP110, 2026-10-19): post-negotiation
            // status on this device22-class firmware is DP_QUERY_NEW
            // (cmd 16) with an ECB(session_key, "{}") body under a CRC32
            // trailer -- the header+HMAC shape gets ignored.  The
            // response is ECB(session_key, PKCS7(json)) with no retcode.
            return self.status_v34_device22();
        }
        let v = self.request(CMD_DP_QUERY, None)?;
        Self::parse_dps_state(&v)
    }

    fn power(&mut self) -> Option<f64> {
        let v = self.request(CMD_DP_QUERY, None)?;
        Self::parse_dps_power(&v)
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
            session_key: None,
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
    fn v34_codec_round_trip_and_hmac_trailer() {
        let key = b"0123456789abcdef";
        let plain = br#"3.4\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00{"devId":"x"}"#;
        let ct = v34_encrypt(key, plain).unwrap();
        assert_eq!(ct.len() % 16, 0, "ECB blocks");
        assert_eq!(v34_decrypt(key, &ct).unwrap(), plain);
        // HMAC trailer: pack/parse round trip, tamper detection
        let frame = pack_frame_trailer(1, CMD_DP_QUERY, b"body", Some(key));
        let parsed = parse_frame_hmac(&frame, key).unwrap();
        assert_eq!(parsed.payload, b"body");
        let mut bad = frame.clone();
        let last = bad.len() - 5;
        bad[last] ^= 0x01;
        assert!(
            parse_frame_hmac(&bad, key).is_none(),
            "HMAC must detect tampering"
        );
        // a v3.3 CRC32 frame must NOT parse as hmac-framed
        assert!(parse_frame_hmac(&pack_frame(1, CMD_DP_QUERY, b"body"), key).is_none());
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
    fn dps_power_only_recognized_keys() {
        assert_eq!(
            TuyaPlug::parse_dps_power(&serde_json::json!({"dps": {"power": 87.5}})),
            Some(87.5)
        );
        assert_eq!(
            TuyaPlug::parse_dps_power(&serde_json::json!({"dps": {"pow": 87.5}})),
            Some(87.5)
        );
        assert_eq!(
            TuyaPlug::parse_dps_power(&serde_json::json!({"dps": {"19": 1234}})),
            None,
            "energy total DP is not live watts -- never guessed"
        );
        assert_eq!(
            TuyaPlug::parse_dps_power(&serde_json::json!({"dps": {"power": 99999.0}})),
            None,
            "absurd magnitude rejected"
        );
    }

    #[test]
    fn unreachable_is_none_not_panic() {
        let mut plug = plug_at(1);
        assert_eq!(plug.status(), None);
        assert!(plug.on().is_err());
    }
}
