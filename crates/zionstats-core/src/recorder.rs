//! Recorder + replay plumbing — the Python-recorder frame shape, exactly.
//!
//! One JSON object per line: `{"ts": float, "env": {...}, "devices":
//! [snake_case Sample fields, null for None], "processes": [GpuProcess
//! fields]}`.  The Rust `Sample` carries a SUBSET of the Python fields
//! (no arch/compute_cap/bar1/mig/display_active/persistence/compute_mode
//! yet): the Python `ReplayProvider` filters traces to its known
//! dataclass fields and defaults the rest, so a Rust-recorded trace
//! replays in Python as-is.  The reverse is NOT true — a Python trace
//! replays in Rust with the extra fields ignored.  Documented
//! one-directional richness.
//!
//! Gzip: a `.gz` suffix selects gzip (flate2, pure-Rust backend), matching
//! the Python recorder's transparent convention.

use std::io::Write;
use std::path::Path;

use serde_json::json;

use crate::types::{GpuProc, ProcKind, Sample};

fn sample_json(s: &Sample, ts_unix: f64) -> serde_json::Value {
    json!({
        "ts": ts_unix,
        "index": s.index,
        "uuid": s.uuid,
        "name": s.name,
        "vendor": s.vendor,
        "pstate": s.pstate,
        "util_gpu": s.util_gpu,
        "util_mem": s.util_mem,
        "util_enc": s.util_enc,
        "util_dec": s.util_dec,
        "mem_used": s.mem_used,
        "mem_total": s.mem_total,
        "power_w": s.power_w,
        "power_limit_w": s.power_limit_w,
        "clocks_sm_mhz": s.clocks_sm_mhz,
        "clocks_mem_mhz": s.clocks_mem_mhz,
        "clocks_max_sm_mhz": s.clocks_max_sm_mhz,
        "energy_mj": s.energy_mj,
        "throttle_bits": s.throttle_bits,
        "throttle": s.throttle,
        "temp_gpu": s.temp_gpu,
        "temp_mem": s.temp_mem,
        "temp_slowdown": s.temp_slowdown,
        "temp_shutdown": s.temp_shutdown,
        "fan_pct": s.fan_pct,
        "pcie_gen_cur": s.pcie_gen_cur,
        "pcie_gen_max": s.pcie_gen_max,
        "pcie_width_cur": s.pcie_width_cur,
        "pcie_width_max": s.pcie_width_max,
        "pcie_rx_kbps": s.pcie_rx_kbps,
        "pcie_tx_kbps": s.pcie_tx_kbps,
        "ecc_sbe": s.ecc_sbe,
        "ecc_dbe": s.ecc_dbe,
    })
}

fn kind_str(k: ProcKind) -> &'static str {
    match k {
        ProcKind::Compute => "C",
        ProcKind::Graphics => "G",
        ProcKind::ComputeAndGraphics => "C+G",
    }
}

fn proc_json(p: &GpuProc) -> serde_json::Value {
    json!({
        "pid": p.pid,
        "gpu_index": p.gpu_index,
        "type": kind_str(p.kind),
        "mem_used": p.mem_used,
        "sm_pct": p.sm_pct,
        "mem_pct": p.mem_pct,
        "enc_pct": p.enc_pct,
        "dec_pct": p.dec_pct,
        "cpu_pct": p.cpu_pct,
        "user": p.user,
        "command": p.command,
    })
}

/// Serialize one frame exactly like the Python `frame_to_dict`.
pub fn frame_to_line(
    env: &crate::types::Environment,
    samples: &[Sample],
    procs: &[GpuProc],
) -> String {
    let ts = samples
        .first()
        .map(|s| {
            s.ts.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        })
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        });
    json!({
        "ts": ts,
        "env": {
            "hostname": env.hostname,
            "kernel": env.kernel,
            "driver": env.driver,
            "nvml": env.nvml,
            "cuda_max": env.cuda_max,
            "provider": env.provider,
        },
        "devices": samples.iter().map(|s| sample_json(s, ts)).collect::<Vec<_>>(),
        "processes": procs.iter().map(proc_json).collect::<Vec<_>>(),
    })
    .to_string()
}

enum Sink {
    Plain(std::io::BufWriter<std::fs::File>),
    Gz(Box<flate2::write::GzEncoder<std::fs::File>>),
}

/// Append-only trace writer; flushes every frame (a crash keeps the
/// frames written so far).
pub struct Recorder {
    sink: Sink,
}

impl Recorder {
    pub fn create(path: &Path) -> Result<Recorder, String> {
        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        let sink = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
            Sink::Gz(Box::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::fast(),
            )))
        } else {
            Sink::Plain(std::io::BufWriter::new(file))
        };
        Ok(Recorder { sink })
    }

    pub fn write(&mut self, line: &str) -> Result<(), String> {
        match &mut self.sink {
            Sink::Plain(w) => {
                writeln!(w, "{line}").map_err(|e| e.to_string())?;
                w.flush().map_err(|e| e.to_string())
            }
            Sink::Gz(w) => {
                writeln!(w, "{line}").map_err(|e| e.to_string())?;
                w.flush().map_err(|e| e.to_string())
            }
        }
    }
}

/// Rust-recorded schema assertion: every device key must be a documented
/// Python `Sample` dataclass field, so the Python replay filter accepts
/// the whole trace (transcribed from the sibling dataclass; keep in sync).
pub const PYTHON_SAMPLE_FIELDS: &[&str] = &[
    "ts",
    "index",
    "uuid",
    "name",
    "arch",
    "compute_cap",
    "pstate",
    "vendor",
    "util_gpu",
    "util_mem",
    "util_enc",
    "util_dec",
    "mem_used",
    "mem_total",
    "mem_reserved",
    "bar1_used",
    "bar1_total",
    "power_w",
    "power_limit_w",
    "power_default_w",
    "power_min_w",
    "power_max_w",
    "clocks_sm_mhz",
    "clocks_mem_mhz",
    "clocks_video_mhz",
    "clocks_max_sm_mhz",
    "energy_mj",
    "throttle_bits",
    "throttle",
    "throttle_counters",
    "temp_gpu",
    "temp_gpu_max",
    "temp_mem",
    "temp_slowdown",
    "temp_shutdown",
    "fan_pct",
    "fan_rpm",
    "pcie_gen_cur",
    "pcie_gen_max",
    "pcie_width_cur",
    "pcie_width_max",
    "pcie_rx_kbps",
    "pcie_tx_kbps",
    "pcie_replay",
    "ecc_sbe",
    "ecc_dbe",
    "mig_mode",
    "mig_instances",
    "display_active",
    "persistence",
    "compute_mode",
];

pub const PYTHON_PROCESS_FIELDS: &[&str] = &[
    "pid",
    "gpu_index",
    "type",
    "mem_used",
    "sm_pct",
    "mem_pct",
    "enc_pct",
    "dec_pct",
    "cpu_pct",
    "user",
    "command",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn s() -> Sample {
        Sample {
            ts: UNIX_EPOCH + Duration::from_secs_f64(1000.0),
            index: 0,
            uuid: "GPU-x".into(),
            name: "Test GPU".into(),
            vendor: "nvidia".into(),
            util_gpu: Some(42.0),
            mem_used: 1024,
            mem_total: 2048,
            power_w: Some(100.0),
            ..Default::default()
        }
    }

    #[test]
    fn frame_shape_matches_python() {
        let env = crate::types::Environment {
            hostname: "box".into(),
            kernel: "Linux 6".into(),
            driver: "580".into(),
            nvml: "13".into(),
            cuda_max: "13.0".into(),
            provider: "nvml",
        };
        let line = frame_to_line(&env, &[s()], &[]);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        // top-level keys exactly like the Python frame (order-insensitive:
        // serde_json maps sort keys; the Python loader reads by name)
        let keys: std::collections::HashSet<&str> =
            v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            ["ts", "env", "devices", "processes"].into_iter().collect()
        );
        assert_eq!(v["ts"], json!(1000.0));
        assert_eq!(v["env"]["provider"], "nvml");
        let dev = &v["devices"][0];
        for (k, _) in dev.as_object().unwrap() {
            assert!(
                PYTHON_SAMPLE_FIELDS.contains(&k.as_str()),
                "device key {k} is not a Python Sample field -- the replay filter would drop the frame silently"
            );
        }
        assert_eq!(dev["util_gpu"], json!(42.0));
        assert_eq!(dev["pstate"], serde_json::Value::Null, "None -> null");
    }

    #[test]
    fn process_keys_match_python() {
        let p = GpuProc {
            pid: 7,
            gpu_index: 0,
            kind: ProcKind::Compute,
            mem_used: 1,
            sm_pct: Some(9.0),
            mem_pct: None,
            enc_pct: None,
            dec_pct: None,
            cpu_pct: None,
            uid: 0,
            user: "u".into(),
            command: "c".into(),
            start_time: 0,
        };
        let env = crate::types::Environment::default();
        let line = frame_to_line(&env, &[], &[p]);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        for (k, _) in v["processes"][0].as_object().unwrap() {
            assert!(
                PYTHON_PROCESS_FIELDS.contains(&k.as_str()),
                "process key {k} is not a Python GpuProcess field"
            );
        }
        assert_eq!(v["processes"][0]["type"], "C");
    }

    #[test]
    fn recorder_gzip_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.ndjson.gz");
        {
            let mut rec = Recorder::create(&path).unwrap();
            rec.write("{\"a\":1}").unwrap();
            rec.write("{\"a\":2}").unwrap();
        }
        let decoder = flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap());
        let text: String = std::io::read_to_string(decoder).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, ["{\"a\":1}", "{\"a\":2}"]);
    }

    #[test]
    fn trace_frame_from_value() {
        let line = r#"{"ts":1.0,"env":{"hostname":"h","kernel":"k","driver":"d","nvml":"n","cuda_max":"c","provider":"demo"},"devices":[{"ts":1.0,"index":0,"uuid":"u","name":"n","util_gpu":5.0}],"processes":[]}"#;
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let frame = TraceFrame::from_value(&v).unwrap();
        assert_eq!(frame.env.provider, "demo");
        assert_eq!(frame.samples.len(), 1);
        assert_eq!(frame.samples[0].util_gpu, Some(5.0));
        assert_eq!(frame.samples[0].mem_used, 0, "missing keys default");
    }
}
