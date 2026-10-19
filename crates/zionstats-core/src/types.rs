//! Shared data types mirroring the Python `Sample`/`GpuProcess` surface.
//! `Option` fields mirror Python `None`; SHM uses NaN for the same purpose.

use std::time::SystemTime;

pub const MAX_GPUS: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct Environment {
    pub hostname: String,
    pub kernel: String,
    pub driver: String,
    pub nvml: String,
    pub cuda_max: String,
    pub provider: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub index: u32,
    pub name: String,
    pub uuid: String,
}

/// Mirror of Python `nvml_layer.Sample` fields used by v0.5. `None` =
/// unsupported/unknown (never a hard error).
#[derive(Debug, Clone)]
pub struct Sample {
    pub ts: SystemTime,
    pub index: u32,
    pub uuid: String,
    pub name: String,
    /// nvidia | amd | intel | demo (empty = unknown)
    pub vendor: String,
    pub pstate: Option<i8>,
    pub util_gpu: Option<f32>,
    pub util_mem: Option<f32>,
    pub util_enc: Option<f32>,
    pub util_dec: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub power_w: Option<f32>,
    pub power_limit_w: Option<f32>,
    pub clocks_sm_mhz: Option<u32>,
    pub clocks_mem_mhz: Option<u32>,
    pub clocks_max_sm_mhz: Option<u32>,
    pub energy_mj: Option<u64>,
    pub throttle_bits: u32,
    pub throttle: Vec<String>,
    pub temp_gpu: Option<f32>,
    /// hottest-part sensor (junction/hotspot); None when absent
    pub temp_gpu_max: Option<f32>,
    pub temp_mem: Option<f32>,
    pub temp_slowdown: Option<f32>,
    pub temp_shutdown: Option<f32>,
    pub fan_pct: Option<f32>,
    pub pcie_gen_cur: Option<u8>,
    pub pcie_gen_max: Option<u8>,
    pub pcie_width_cur: Option<u8>,
    pub pcie_width_max: Option<u8>,
    pub pcie_rx_kbps: Option<u32>,
    pub pcie_tx_kbps: Option<u32>,
    pub ecc_sbe: Option<u64>,
    pub ecc_dbe: Option<u64>,
}

impl Default for Sample {
    fn default() -> Self {
        Sample {
            ts: SystemTime::UNIX_EPOCH,
            index: 0,
            uuid: String::new(),
            name: String::new(),
            vendor: String::new(),
            temp_gpu_max: None,
            pstate: None,
            util_gpu: None,
            util_mem: None,
            util_enc: None,
            util_dec: None,
            mem_used: 0,
            mem_total: 0,
            power_w: None,
            power_limit_w: None,
            clocks_sm_mhz: None,
            clocks_mem_mhz: None,
            clocks_max_sm_mhz: None,
            energy_mj: None,
            throttle_bits: 0,
            throttle: Vec::new(),
            temp_gpu: None,
            temp_mem: None,
            temp_slowdown: None,
            temp_shutdown: None,
            fan_pct: None,
            pcie_gen_cur: None,
            pcie_gen_max: None,
            pcie_width_cur: None,
            pcie_width_max: None,
            pcie_rx_kbps: None,
            pcie_tx_kbps: None,
            ecc_sbe: None,
            ecc_dbe: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcKind {
    Compute,
    Graphics,
    ComputeAndGraphics,
}

impl ProcKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcKind::Compute => "C",
            ProcKind::Graphics => "G",
            ProcKind::ComputeAndGraphics => "C+G",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GpuProc {
    pub pid: u32,
    pub gpu_index: u32,
    pub kind: ProcKind,
    pub mem_used: u64,
    pub sm_pct: Option<f32>,
    pub mem_pct: Option<f32>,
    pub enc_pct: Option<f32>,
    pub dec_pct: Option<f32>,
    pub cpu_pct: Option<f32>,
    pub uid: u32,
    pub user: String,
    pub command: String,
    /// /proc/<pid>/stat starttime (field 22) — PID-reuse defense key.
    pub start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Crit,
}

/// Pure verdict used for the SHM badge. Simpler than the Python insight
/// layer by design (documented delta): Python keeps the rich verdict chips;
/// the Rust side carries a traffic-light only. GPU-Idle / App-Clocks /
/// Sync-Boost / Display-Clocks are benign states, not warnings.
pub fn verdict_of(s: &Sample) -> Verdict {
    const HW_BITS: u32 = 0x008 | 0x040 | 0x080; // HW-Slowdown | HW-Thermal | HW-Brake
    const WARN_BITS: u32 = 0x004 | 0x020; // SW-Power-Cap | SW-Thermal
    if let Some(shutdown) = s.temp_shutdown {
        if s.temp_gpu.is_some_and(|t| t >= shutdown - 5.0) {
            return Verdict::Crit;
        }
    }
    if let Some(slowdown) = s.temp_slowdown {
        if s.temp_gpu.is_some_and(|t| t >= slowdown) {
            return Verdict::Crit;
        }
    }
    if s.throttle_bits & HW_BITS != 0 {
        return Verdict::Crit;
    }
    if let Some(slowdown) = s.temp_slowdown {
        if s.temp_gpu.is_some_and(|t| t >= slowdown - 5.0) {
            return Verdict::Warn;
        }
    }
    if s.throttle_bits & WARN_BITS != 0 {
        return Verdict::Warn;
    }
    Verdict::Ok
}

/// nvml.h clocksThrottleReasons bits (stable ABI), for decode helpers.
pub const THROTTLE_BITS: &[(u32, &str)] = &[
    (0x001, "GPU-Idle"),
    (0x002, "App-Clocks"),
    (0x004, "SW-Power-Cap"),
    (0x008, "HW-Slowdown"),
    (0x010, "Sync-Boost"),
    (0x020, "SW-Thermal"),
    (0x040, "HW-Thermal"),
    (0x080, "HW-Brake"),
    (0x100, "Display-Clocks"),
];

pub fn decode_throttle(bits: u32) -> Vec<&'static str> {
    THROTTLE_BITS
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(throttle: u32, temp: Option<f32>, slowdown: Option<f32>) -> Sample {
        Sample {
            throttle_bits: throttle,
            temp_gpu: temp,
            temp_slowdown: slowdown,
            temp_shutdown: Some(98.0),
            ..Default::default()
        }
    }

    #[test]
    fn verdict_levels() {
        assert_eq!(verdict_of(&s(0, Some(40.0), Some(90.0))), Verdict::Ok);
        assert_eq!(verdict_of(&s(0x004, Some(40.0), Some(90.0))), Verdict::Warn);
        assert_eq!(verdict_of(&s(0, Some(86.0), Some(90.0))), Verdict::Warn);
        assert_eq!(verdict_of(&s(0, Some(90.0), Some(90.0))), Verdict::Crit);
        assert_eq!(verdict_of(&s(0x040, Some(40.0), Some(90.0))), Verdict::Crit);
        assert_eq!(verdict_of(&s(0, Some(94.0), Some(90.0))), Verdict::Crit);
    }

    #[test]
    fn throttle_decode() {
        assert_eq!(decode_throttle(0x005), vec!["GPU-Idle", "SW-Power-Cap"]);
        assert!(decode_throttle(0).is_empty());
    }
}
