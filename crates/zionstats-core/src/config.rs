//! Tolerant config reader — same file as the Python app
//! (`~/.config/zionstats/config.toml`, `$ZION_CONFIG` override), same
//! `_coerce` philosophy: every key individually garbage-tolerant, unknown
//! keys ignored, ANY failure falls back to defaults and never panics.
//! The Rust side never writes the config (Python stays the writer).

use crate::control::curve::{AGGRESSIVE_FAN_CURVE, DEFAULT_FAN_CURVE};
use crate::control::{ControlConfig, CoolingMode, Trigger};

#[derive(Debug, Clone)]
pub struct FanPreset {
    pub name: &'static str,
    pub curve: &'static [(f32, u32)],
}

pub const FAN_PRESETS: &[FanPreset] = &[
    FanPreset {
        name: "default",
        curve: DEFAULT_FAN_CURVE,
    },
    // directive example: engage early, back off late
    FanPreset {
        name: "aggressive",
        curve: AGGRESSIVE_FAN_CURVE,
    },
];

#[derive(Debug, Clone, PartialEq)]
pub struct PlugCfg {
    pub backend: String,
    pub name: String,
    pub host: String,
    pub gen: u8,
    pub channel: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub tui_interval_s: f64,
    pub readonly: bool,
    pub daemon: DaemonCfg,
    pub cooling: ControlConfig,
    pub plugs: Vec<PlugCfg>,
    pub expose_processes: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DaemonCfg {
    pub interval_s: f64,
    pub host: String,
    pub port: u16,
    pub readonly: bool,
    pub ambient_temp_c: f32,
    pub fan_curve: Vec<(f32, u32)>,
    pub fan_pwm_step: u32,
    /// what to do with plugs on SIGTERM: "auto_off" | "keep"
    pub shutdown_plug_policy: String,
    /// optional group chown for the SHM file (root-daemon / user-TUI setup)
    pub shm_group: Option<String>,
    /// opt-in direct GPU fan-PWM control (nvml manual handoff)
    pub fan_control: bool,
}

impl Default for DaemonCfg {
    fn default() -> Self {
        DaemonCfg {
            interval_s: 1.0,
            host: "127.0.0.1".to_string(),
            port: 9400,
            readonly: false,
            ambient_temp_c: 30.0,
            fan_curve: DEFAULT_FAN_CURVE.to_vec(),
            fan_pwm_step: 3,
            shutdown_plug_policy: "auto_off".to_string(),
            shm_group: None,
            fan_control: false,
        }
    }
}

fn as_f64(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Float(f) => Some(*f),
        toml::Value::Integer(i) => Some(*i as f64),
        toml::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_bool(v: &toml::Value) -> Option<bool> {
    match v {
        toml::Value::Boolean(b) => Some(*b),
        toml::Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn as_string(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            tui_interval_s: 0.25,
            readonly: false,
            daemon: DaemonCfg::default(),
            cooling: ControlConfig::default(),
            plugs: Vec::new(),
            expose_processes: true,
        }
    }
}

/// Load with full garbage tolerance: a malformed file, wrong-typed keys,
/// unknown keys — all silently fall back to defaults per-key.
pub fn load(path: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return cfg;
    };
    let Ok(data) = text.parse::<toml::Table>() else {
        return cfg;
    };

    // top-level flat keys shared with the Python app
    if let Some(v) = data.get("interval").and_then(as_f64) {
        cfg.tui_interval_s = v.clamp(0.25, 30.0);
    }
    if let Some(v) = data.get("readonly").and_then(as_bool) {
        cfg.readonly = v;
    }

    // [cooling] — same keys/defaults as Python config_from_section
    if let Some(sec) = data.get("cooling").and_then(|v| v.as_table()) {
        let c = &mut cfg.cooling;
        if let Some(v) = sec.get("mode").and_then(as_string) {
            if let Some(m) = CoolingMode::parse(&v) {
                c.mode = m;
            }
        }
        if let Some(v) = sec.get("trigger").and_then(as_string) {
            if v == "usage" {
                c.trigger = Trigger::Usage;
            } else if v == "temp" {
                c.trigger = Trigger::Temp;
            }
        }
        let num = |k: &str| sec.get(k).and_then(as_f64).map(|f| f as f32);
        if let Some(v) = num("trigger_temp") {
            c.trigger_temp = v;
        }
        if let Some(v) = num("resume_temp") {
            c.resume_temp = v;
        }
        if let Some(v) = num("min_delta_c") {
            c.min_delta_c = v;
        }
        if let Some(v) = num("min_on_s") {
            c.min_on_s = v;
        }
        if let Some(v) = num("min_off_s") {
            c.min_off_s = v;
        }
        if let Some(v) = num("max_duty_min") {
            c.max_duty_min = v;
        }
        if let Some(v) = num("usage_util") {
            c.usage_util = v;
        }
        if let Some(v) = num("idle_off_s") {
            c.idle_off_s = v;
        }
    }

    // [[cooling.plugs]]
    if let Some(plugs) = data
        .get("cooling")
        .and_then(|c| c.get("plugs"))
        .and_then(|p| p.as_array())
    {
        for p in plugs {
            let Some(t) = p.as_table() else { continue };
            let backend = t.get("backend").and_then(as_string).unwrap_or_default();
            let name = t
                .get("name")
                .and_then(as_string)
                .unwrap_or_else(|| "plug".to_string());
            let host = t.get("host").and_then(as_string).unwrap_or_default();
            let gen = t.get("gen").and_then(as_f64).map(|f| f as u8).unwrap_or(1);
            let channel = t
                .get("channel")
                .and_then(as_f64)
                .map(|f| f as u32)
                .unwrap_or(0);
            cfg.plugs.push(PlugCfg {
                backend,
                name,
                host,
                gen,
                channel,
            });
        }
    }

    // [export] — expose_processes honored; host/port are the PYTHON
    // exporter's (port 9835); the daemon's socket lives in [daemon]
    if let Some(sec) = data.get("export").and_then(|v| v.as_table()) {
        if let Some(v) = sec.get("expose_processes").and_then(as_bool) {
            cfg.expose_processes = v;
        }
    }

    // [daemon]
    if let Some(sec) = data.get("daemon").and_then(|v| v.as_table()) {
        let d = &mut cfg.daemon;
        if let Some(v) = sec.get("interval").and_then(as_f64) {
            d.interval_s = v.clamp(0.25, 30.0);
        }
        if let Some(v) = sec.get("host").and_then(as_string) {
            d.host = v;
        }
        if let Some(v) = sec.get("port").and_then(as_f64) {
            let p = v as u32;
            if (1..=65535).contains(&p) {
                d.port = p as u16;
            }
        }
        if let Some(v) = sec.get("readonly").and_then(as_bool) {
            d.readonly = v;
        }
        if let Some(v) = sec.get("ambient_temp").and_then(as_f64) {
            if (-100.0..=150.0).contains(&v) {
                d.ambient_temp_c = v as f32;
            }
        }
        if let Some(v) = sec.get("fan_pwm_step").and_then(as_f64) {
            let s = v as u32;
            if (1..=50).contains(&s) {
                d.fan_pwm_step = s;
            }
        }
        if let Some(v) = sec.get("shutdown_plug_policy").and_then(as_string) {
            if v == "auto_off" || v == "keep" {
                d.shutdown_plug_policy = v;
            }
        }
        if let Some(v) = sec.get("shm_group").and_then(as_string) {
            d.shm_group = Some(v);
        }
        if let Some(v) = sec.get("fan_control").and_then(as_bool) {
            d.fan_control = v;
        }
        if let Some(v) = sec.get("fan_curve").and_then(|c| c.as_array()) {
            let mut curve = Vec::new();
            for pt in v {
                if let Some(pair) = pt.as_array() {
                    if pair.len() == 2 {
                        if let (Some(t), Some(p)) = (as_f64(&pair[0]), as_f64(&pair[1])) {
                            curve.push((t as f32, (p as u32).min(100)));
                        }
                    }
                }
            }
            if curve.len() >= 2 {
                d.fan_curve = curve;
            }
        }
    }

    cfg
}

/// `$ZION_CONFIG` -> explicit path -> default location.
pub fn config_path(explicit: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("ZION_CONFIG") {
        if !p.is_empty() {
            return p.into();
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    std::path::Path::new(&home)
        .join(".config")
        .join("zionstats")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(toml_text: &str) -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, toml_text).unwrap();
        // leak the dir so the path stays valid for the test duration
        std::mem::forget(dir);
        p
    }

    #[test]
    fn defaults_when_file_missing() {
        let cfg = load(std::path::Path::new("/nonexistent/zion.toml"));
        assert_eq!(cfg.daemon.port, 9400);
        assert_eq!(cfg.cooling.trigger_temp, 82.0);
        assert_eq!(cfg.cooling.mode, CoolingMode::Off);
    }

    #[test]
    fn garbage_file_falls_back() {
        let cfg = load(&write("this is not toml {{{{"));
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn cooling_section_matches_python_defaults() {
        let cfg = load(&write(
            "[cooling]\nenabled = true\nmode = \"dry_run\"\ntrigger = \"usage\"\nusage_util = 8\nidle_off_s = 300\n",
        ));
        assert_eq!(cfg.cooling.mode, CoolingMode::DryRun);
        assert_eq!(cfg.cooling.trigger, Trigger::Usage);
        assert_eq!(cfg.cooling.usage_util, 8.0);
        assert_eq!(cfg.cooling.idle_off_s, 300.0);
        assert_eq!(cfg.cooling.trigger_temp, 82.0, "unset keys keep defaults");
    }

    #[test]
    fn wrong_types_coerced_or_defaulted() {
        let cfg = load(&write(
            "[cooling]\ntrigger_temp = \"hot\"\nusage_util = true\ninterval = \"fast\"\n[daemon]\nport = \"http\"\n",
        ));
        assert_eq!(cfg.cooling.trigger_temp, 82.0, "garbage string -> default");
        assert_eq!(cfg.daemon.port, 9400, "garbage port -> default");
        assert_eq!(cfg.tui_interval_s, 0.25, "garbage interval -> default");
    }

    #[test]
    fn daemon_section_and_fan_curve() {
        let cfg = load(&write(
            "[daemon]\ninterval = 0.25\nport = 9401\nreadonly = true\nambient_temp = 24.5\nfan_pwm_step = 5\nfan_curve = [[40, 0], [60, 50], [80, 100]]\n",
        ));
        assert_eq!(cfg.daemon.interval_s, 0.25);
        assert_eq!(cfg.daemon.port, 9401);
        assert!(cfg.daemon.readonly);
        assert_eq!(cfg.daemon.ambient_temp_c, 24.5);
        assert_eq!(cfg.daemon.fan_pwm_step, 5);
        assert_eq!(cfg.daemon.fan_curve.len(), 3);
        assert_eq!(cfg.daemon.fan_curve[1], (60.0, 50));
    }

    #[test]
    fn plugs_parsed_tuya_flagged_but_kept() {
        let cfg = load(&write(
            "[[cooling.plugs]]\nbackend = \"shelly\"\nname = \"blower\"\nhost = \"192.168.1.50\"\ngen = 2\nchannel = 0\n",
        ));
        assert_eq!(cfg.plugs.len(), 1);
        assert_eq!(cfg.plugs[0].backend, "shelly");
        assert_eq!(cfg.plugs[0].gen, 2);
    }

    #[test]
    fn bool_coercion_strings() {
        let cfg = load(&write("readonly = \"yes\"\n"));
        assert!(cfg.readonly);
    }
}
