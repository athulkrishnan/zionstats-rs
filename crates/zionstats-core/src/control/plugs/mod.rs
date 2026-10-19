//! Plug backends: HTTP (Shelly Gen1/Gen2 + Tasmota), Kasa (raw TCP),
//! Tuya (local v3.3 + v3.4), Home Assistant (REST). All LAN-local; the
//! network posture is unchanged from the Python backends (plain local
//! calls, no cloud). Wire formats are verified against the reference
//! clients at implementation time and pinned by mock-server tests here.

pub mod ha;
pub mod http;
pub mod kasa;

use super::actuators::Plug;
use crate::config::PlugCfg;

/// Build the backend plug for one config entry. `Ok(None)` = skippable
/// (optional fields absent -- the caller warns); `Err` = unknown backend
/// or unusable config.
pub fn from_config(cfg: &PlugCfg) -> Result<Option<Box<dyn Plug>>, String> {
    match cfg.backend.as_str() {
        "shelly" | "tasmota" => match http::HttpPlug::from_config(cfg)? {
            Some(p) => Ok(Some(Box::new(p))),
            None => Ok(None),
        },
        // landed in the next commit (tuya bridge)
        "tuya" => Ok(None),
        "kasa" => match kasa::KasaPlug::from_config(cfg)? {
            Some(p) => Ok(Some(Box::new(p))),
            None => Ok(None),
        },
        "ha" => match ha::HaPlug::from_config(cfg)? {
            Some(p) => Ok(Some(Box::new(p))),
            None => Ok(None),
        },
        other => Err(format!("unknown plug backend {other:?}")),
    }
}
