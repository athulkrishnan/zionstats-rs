//! Replay source: feeds a recorded trace (Python-compatible frame shape,
//! see `crate::recorder`) through the `Source` trait.  Frames advance one
//! per `sample()` call; at exhaustion the LAST frame is held (the TUI
//! keeps rendering a frozen replay instead of going blank).  Honors the
//! SHM flock upstream, so a live daemon always wins the attach race.

use std::path::Path;

use super::{Source, SourceError};
use crate::types::{DeviceInfo, Environment, GpuProc, Sample};

#[derive(Debug, Clone, Default)]
pub struct TraceFrame {
    pub env: Environment,
    pub samples: Vec<Sample>,
    pub procs: Vec<GpuProc>,
}

fn f64_of(v: &serde_json::Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

impl TraceFrame {
    /// Python-replay-filter parity: known keys parsed, unknown keys and
    /// missing keys tolerated (defaults).
    pub fn from_value(v: &serde_json::Value) -> Option<TraceFrame> {
        let env_v = v.get("env")?;
        let g = |k: &str| {
            env_v
                .get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let provider = match env_v.get("provider").and_then(|x| x.as_str()) {
            Some("nvml") => "nvml",
            Some("demo") => "demo",
            Some("amd") => "amd",
            Some("intel") => "intel",
            _ => "replay",
        };
        let env = Environment {
            hostname: g("hostname"),
            kernel: g("kernel"),
            driver: g("driver"),
            nvml: g("nvml"),
            cuda_max: g("cuda_max"),
            provider,
        };
        let mut samples = Vec::new();
        for d in v
            .get("devices")
            .and_then(|x| x.as_array())
            .unwrap_or(&vec![])
        {
            let ts = f64_of(d, "ts").unwrap_or(0.0);
            samples.push(Sample {
                ts: std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(ts),
                index: d.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                uuid: d
                    .get("uuid")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .into(),
                name: d.get("name").and_then(|x| x.as_str()).unwrap_or("?").into(),
                vendor: d
                    .get("vendor")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .into(),
                pstate: d.get("pstate").and_then(|x| x.as_i64()).map(|p| p as i8),
                util_gpu: d.get("util_gpu").and_then(|x| x.as_f64()).map(|f| f as f32),
                util_mem: d.get("util_mem").and_then(|x| x.as_f64()).map(|f| f as f32),
                util_enc: d.get("util_enc").and_then(|x| x.as_f64()).map(|f| f as f32),
                util_dec: d.get("util_dec").and_then(|x| x.as_f64()).map(|f| f as f32),
                mem_used: d.get("mem_used").and_then(|x| x.as_u64()).unwrap_or(0),
                mem_total: d.get("mem_total").and_then(|x| x.as_u64()).unwrap_or(0),
                power_w: d.get("power_w").and_then(|x| x.as_f64()).map(|f| f as f32),
                power_limit_w: d
                    .get("power_limit_w")
                    .and_then(|x| x.as_f64())
                    .map(|f| f as f32),
                clocks_sm_mhz: d
                    .get("clocks_sm_mhz")
                    .and_then(|x| x.as_u64())
                    .map(|c| c as u32),
                clocks_mem_mhz: d
                    .get("clocks_mem_mhz")
                    .and_then(|x| x.as_u64())
                    .map(|c| c as u32),
                clocks_max_sm_mhz: d
                    .get("clocks_max_sm_mhz")
                    .and_then(|x| x.as_u64())
                    .map(|c| c as u32),
                energy_mj: d.get("energy_mj").and_then(|x| x.as_u64()),
                throttle_bits: d.get("throttle_bits").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                throttle: d
                    .get("throttle")
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                temp_gpu: d.get("temp_gpu").and_then(|x| x.as_f64()).map(|f| f as f32),
                temp_gpu_max: d
                    .get("temp_gpu_max")
                    .and_then(|x| x.as_f64())
                    .map(|f| f as f32),
                temp_mem: d.get("temp_mem").and_then(|x| x.as_f64()).map(|f| f as f32),
                temp_slowdown: d
                    .get("temp_slowdown")
                    .and_then(|x| x.as_f64())
                    .map(|f| f as f32),
                temp_shutdown: d
                    .get("temp_shutdown")
                    .and_then(|x| x.as_f64())
                    .map(|f| f as f32),
                fan_pct: d.get("fan_pct").and_then(|x| x.as_f64()).map(|f| f as f32),
                pcie_gen_cur: d
                    .get("pcie_gen_cur")
                    .and_then(|x| x.as_u64())
                    .map(|g| g as u8),
                pcie_gen_max: d
                    .get("pcie_gen_max")
                    .and_then(|x| x.as_u64())
                    .map(|g| g as u8),
                pcie_width_cur: d
                    .get("pcie_width_cur")
                    .and_then(|x| x.as_u64())
                    .map(|w| w as u8),
                pcie_width_max: d
                    .get("pcie_width_max")
                    .and_then(|x| x.as_u64())
                    .map(|w| w as u8),
                pcie_rx_kbps: d
                    .get("pcie_rx_kbps")
                    .and_then(|x| x.as_u64())
                    .map(|k| k as u32),
                pcie_tx_kbps: d
                    .get("pcie_tx_kbps")
                    .and_then(|x| x.as_u64())
                    .map(|k| k as u32),
                ecc_sbe: d.get("ecc_sbe").and_then(|x| x.as_u64()),
                ecc_dbe: d.get("ecc_dbe").and_then(|x| x.as_u64()),
            });
        }
        let mut procs = Vec::new();
        for p in v
            .get("processes")
            .and_then(|x| x.as_array())
            .unwrap_or(&vec![])
        {
            let kind = match p.get("type").and_then(|x| x.as_str()) {
                Some("G") => crate::types::ProcKind::Graphics,
                Some("C+G") => crate::types::ProcKind::ComputeAndGraphics,
                _ => crate::types::ProcKind::Compute,
            };
            procs.push(GpuProc {
                pid: p.get("pid").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                gpu_index: p.get("gpu_index").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                kind,
                mem_used: p.get("mem_used").and_then(|x| x.as_u64()).unwrap_or(0),
                sm_pct: p.get("sm_pct").and_then(|x| x.as_f64()).map(|f| f as f32),
                mem_pct: p.get("mem_pct").and_then(|x| x.as_f64()).map(|f| f as f32),
                enc_pct: p.get("enc_pct").and_then(|x| x.as_f64()).map(|f| f as f32),
                dec_pct: p.get("dec_pct").and_then(|x| x.as_f64()).map(|f| f as f32),
                cpu_pct: p.get("cpu_pct").and_then(|x| x.as_f64()).map(|f| f as f32),
                uid: 0,
                user: p.get("user").and_then(|x| x.as_str()).unwrap_or("?").into(),
                command: p
                    .get("command")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .into(),
                start_time: 0,
            });
        }
        Some(TraceFrame {
            env,
            samples,
            procs,
        })
    }
}

fn read_lines(path: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        use std::io::BufRead;
        let decoder = flate2::read::GzDecoder::new(file);
        Ok(std::io::BufReader::new(decoder)
            .lines()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?)
    } else {
        use std::io::BufRead;
        Ok(std::io::BufReader::new(file)
            .lines()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?)
    }
}

/// Parse a whole trace file (ndjson or .gz).  Blank/garbage lines are
/// skipped (a partial last line from a crashed recorder still replays).
pub fn load_trace(path: &Path) -> Result<Vec<TraceFrame>, String> {
    let mut frames = Vec::new();
    for line in read_lines(path)? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(f) = TraceFrame::from_value(&v) {
                frames.push(f);
            }
        }
    }
    if frames.is_empty() {
        return Err(format!("no usable frames in {}", path.display()));
    }
    Ok(frames)
}

pub struct ReplaySource {
    frames: Vec<TraceFrame>,
    cursor: usize,
    devices_cache: Vec<DeviceInfo>,
}

impl ReplaySource {
    pub fn from_path(path: &Path) -> Result<ReplaySource, SourceError> {
        let frames = load_trace(path).map_err(SourceError::Nvml)?;
        let devices_cache = frames
            .first()
            .map(|f| {
                f.samples
                    .iter()
                    .map(|s| DeviceInfo {
                        index: s.index,
                        name: s.name.clone(),
                        uuid: s.uuid.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ReplaySource {
            frames,
            cursor: 0,
            devices_cache,
        })
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }
}

impl Source for ReplaySource {
    fn environment(&mut self) -> Environment {
        let mut env = self.frames[0].env.clone();
        env.provider = "replay";
        env
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, SourceError> {
        Ok(self.devices_cache.clone())
    }

    fn sample(&mut self) -> Vec<Sample> {
        let out = self.frames[self.cursor].samples.clone();
        if self.cursor < self.frames.len() - 1 {
            self.cursor += 1;
        }
        out
    }

    fn processes(&mut self) -> Vec<GpuProc> {
        self.frames[self.cursor].procs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_holds_last_frame_at_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.ndjson");
        std::fs::write(
            &path,
            concat!(
                "{\"ts\":1.0,\"env\":{\"provider\":\"demo\"},\"devices\":[{\"index\":0,\"util_gpu\":10.0}],\"processes\":[]}\n",
                "{\"ts\":2.0,\"env\":{\"provider\":\"demo\"},\"devices\":[{\"index\":0,\"util_gpu\":20.0}],\"processes\":[]}\n"
            ),
        )
        .unwrap();
        let mut src = ReplaySource::from_path(&path).unwrap();
        assert_eq!(src.frame_count(), 2);
        assert_eq!(
            src.sample()[0].util_gpu,
            Some(10.0),
            "first frame on first tick"
        );
        assert_eq!(src.sample()[0].util_gpu, Some(20.0));
        assert_eq!(src.sample()[0].util_gpu, Some(20.0), "held at exhaustion");
        assert_eq!(src.environment().provider, "replay");
    }

    #[test]
    fn garbage_tail_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.ndjson");
        std::fs::write(
            &path,
            "{\"ts\":1.0,\"env\":{},\"devices\":[{\"index\":0}],\"processes\":[]}\n{\"trunc",
        )
        .unwrap();
        let src = ReplaySource::from_path(&path).unwrap();
        assert_eq!(src.frame_count(), 1);
    }

    #[test]
    fn python_trace_fixture_replays() {
        // committed fixture: generated by the sibling's
        // `zionstats --demo --record ... --duration 3` (7 frames) -- the
        // cross-compat proof that Rust replays Python traces
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_trace.ndjson.gz");
        let mut src = ReplaySource::from_path(&path).unwrap();
        assert!(src.frame_count() >= 5, "real demo trace");
        let first = src.sample();
        assert!(!first.is_empty());
        assert_eq!(first[0].name, "NVIDIA GeForce RTX 4070 Ti SUPER");
        let procs = src.processes();
        assert!(!procs.is_empty(), "demo trace carries processes");
        // iterate to exhaustion: last frame is held
        for _ in 0..(src.frame_count() + 3) {
            let _ = src.sample();
        }
        assert!(!src.sample().is_empty());
    }

    #[test]
    fn empty_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.ndjson");
        std::fs::write(&path, "").unwrap();
        assert!(ReplaySource::from_path(&path).is_err());
    }
}
