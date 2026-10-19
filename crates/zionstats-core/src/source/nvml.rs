//! Live NVML source. Full coverage via the individual getters (the sys
//! crate does not bind the batched FieldValues API; at 250 ms intervals
//! ~30 microsecond-class calls per GPU are negligible). Contract: a sample
//! never fails wholesale — every field is best-effort `Option` (Python
//! `_g()` parity), so the TUI never dies mid-session.

use std::time::SystemTime;

use nvml_wrapper::enum_wrappers::device::PerformanceState;
use nvml_wrapper::enum_wrappers::device::{
    Clock, EccCounter, MemoryError, PcieUtilCounter, TemperatureSensor, TemperatureThreshold,
};
use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::Nvml;

use super::nvml_pmon::PmonSource;
use super::{Source, SourceError};
use crate::types::{decode_throttle, DeviceInfo, Environment, GpuProc, ProcKind, Sample};

pub struct NvmlSource {
    nvml: &'static Nvml,
    indices: Vec<u32>,
    /// last /proc cpu totals per pid for the CPU% delta
    last_cpu: std::sync::Mutex<std::collections::HashMap<u32, (u64, SystemTime)>>,
    /// raw-NVML per-process utilization (pmon); None when the sym binding
    /// is unavailable on this box
    pmon: Option<super::nvml_pmon::NvmlPmonFfi>,
    pmon_trackers: Vec<super::nvml_pmon::PmonTracker>,
}

// SAFETY: the daemon drives NvmlSource from a single poll task.
unsafe impl Send for NvmlSource {}

impl NvmlSource {
    pub fn try_new(only: Option<&[u32]>) -> Result<NvmlSource, SourceError> {
        let nvml: &'static Nvml = Box::leak(Box::new(
            Nvml::init().map_err(|e| SourceError::Nvml(e.to_string()))?,
        ));
        let count = nvml
            .device_count()
            .map_err(|e| SourceError::Nvml(e.to_string()))?;
        let mut indices = Vec::new();
        for i in 0..count {
            if let Some(only) = only {
                if !only.contains(&i) {
                    continue;
                }
            }
            indices.push(i);
        }
        if indices.is_empty() {
            return Err(SourceError::NoDevices);
        }
        let pmon = super::nvml_pmon::NvmlPmonFfi::try_new();
        let n = indices.len();
        Ok(NvmlSource {
            nvml,
            indices,
            last_cpu: std::sync::Mutex::new(std::collections::HashMap::new()),
            pmon,
            pmon_trackers: (0..n).map(|_| Default::default()).collect(),
        })
    }
}

fn hostname_of() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default()
}

fn kernel_of() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|k| format!("Linux {}", k.trim()))
        .unwrap_or_default()
}

impl Source for NvmlSource {
    fn environment(&mut self) -> Environment {
        let driver = self.nvml.sys_driver_version().unwrap_or_default();
        let nvml_ver = self.nvml.sys_nvml_version().unwrap_or_default();
        let cuda_max = self
            .nvml
            .sys_cuda_driver_version()
            .map(|v| format!("{}.{}", v / 1000, (v % 1000) / 10))
            .unwrap_or_default();
        Environment {
            hostname: hostname_of(),
            kernel: kernel_of(),
            driver,
            nvml: nvml_ver,
            cuda_max,
            provider: "nvml",
        }
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, SourceError> {
        Ok(self
            .indices
            .iter()
            .map(|i| match self.nvml.device_by_index(*i) {
                Ok(d) => DeviceInfo {
                    index: *i,
                    name: d.name().unwrap_or_else(|_| "?".into()),
                    uuid: d.uuid().unwrap_or_default(),
                },
                Err(_) => DeviceInfo {
                    index: *i,
                    name: "?".into(),
                    uuid: String::new(),
                },
            })
            .collect())
    }

    fn sample(&mut self) -> Vec<Sample> {
        let now = SystemTime::now();
        self.indices
            .iter()
            .map(|i| {
                let mut s = Sample {
                    ts: now,
                    index: *i,
                    ..Default::default()
                };
                if let Ok(dev) = self.nvml.device_by_index(*i) {
                    if let Ok(name) = dev.name() {
                        s.name = name;
                    }
                    if let Ok(uuid) = dev.uuid() {
                        s.uuid = uuid;
                    }
                    if let Ok(u) = dev.utilization_rates() {
                        s.util_gpu = Some(u.gpu as f32);
                        s.util_mem = Some(u.memory as f32);
                    }
                    if let Ok(e) = dev.encoder_utilization() {
                        s.util_enc = Some(e.utilization as f32);
                    }
                    if let Ok(dec) = dev.decoder_utilization() {
                        s.util_dec = Some(dec.utilization as f32);
                    }
                    if let Ok(m) = dev.memory_info() {
                        s.mem_used = m.used;
                        s.mem_total = m.total;
                    }
                    s.power_w = dev.power_usage().ok().map(|mw| mw as f32 / 1000.0);
                    s.power_limit_w = dev
                        .enforced_power_limit()
                        .or_else(|_| dev.power_management_limit())
                        .ok()
                        .map(|mw| mw as f32 / 1000.0);
                    s.clocks_sm_mhz = dev.clock_info(Clock::SM).ok();
                    s.clocks_mem_mhz = dev.clock_info(Clock::Memory).ok();
                    s.clocks_max_sm_mhz = dev.max_clock_info(Clock::SM).ok();
                    s.energy_mj = dev.total_energy_consumption().ok();
                    // volatile ECC totals (Python parity); GeForce commonly
                    // returns NotSupported -> honest None
                    s.ecc_sbe = dev
                        .total_ecc_errors(MemoryError::Corrected, EccCounter::Volatile)
                        .ok();
                    s.ecc_dbe = dev
                        .total_ecc_errors(MemoryError::Uncorrected, EccCounter::Volatile)
                        .ok();
                    s.vendor = "nvidia".to_string();
                    if let Ok(bits) = dev.current_throttle_reasons() {
                        s.throttle_bits = bits.bits() as u32;
                        s.throttle = decode_throttle(bits.bits() as u32)
                            .into_iter()
                            .map(String::from)
                            .collect();
                    }
                    s.temp_gpu = dev
                        .temperature(TemperatureSensor::Gpu)
                        .ok()
                        .map(|t| t as f32);
                    s.temp_slowdown = dev
                        .temperature_threshold(TemperatureThreshold::Slowdown)
                        .ok()
                        .map(|t| t as f32);
                    s.temp_shutdown = dev
                        .temperature_threshold(TemperatureThreshold::Shutdown)
                        .ok()
                        .map(|t| t as f32);
                    if let Ok(n) = dev.num_fans() {
                        if n > 0 {
                            s.fan_pct = dev.fan_speed(0).ok().map(|f| f as f32);
                        }
                    }
                    s.pcie_gen_cur = dev.current_pcie_link_gen().ok().map(|g| g as u8);
                    s.pcie_gen_max = dev.max_pcie_link_gen().ok().map(|g| g as u8);
                    s.pcie_width_cur = dev.current_pcie_link_width().ok().map(|w| w as u8);
                    s.pcie_width_max = dev.max_pcie_link_width().ok().map(|w| w as u8);
                    s.pcie_rx_kbps = dev.pcie_throughput(PcieUtilCounter::Receive).ok();
                    s.pcie_tx_kbps = dev.pcie_throughput(PcieUtilCounter::Send).ok();
                    if let Ok(p) = dev.performance_state() {
                        if p != PerformanceState::Unknown {
                            s.pstate = Some(p as i8);
                        }
                    }
                }
                s
            })
            .collect()
    }

    fn processes(&mut self) -> Vec<GpuProc> {
        let mut out: Vec<GpuProc> = Vec::new();
        for (pos, i) in self.indices.iter().enumerate() {
            let Ok(dev) = self.nvml.device_by_index(*i) else {
                continue;
            };
            // per-process utilization via the raw FFI (best-effort: the
            // slots stay None when the sym is absent or the call fails)
            let pmon_by_pid: std::collections::HashMap<u32, super::nvml_pmon::ProcUtilSample> =
                match (self.pmon.as_mut(), self.pmon_trackers.get_mut(pos)) {
                    (Some(pmon), Some(tracker)) => {
                        // SAFETY: the wrapper hands out the raw nvmlDevice_t
                        // it already owns; we pass it straight back to NVML.
                        let raw = unsafe { dev.handle() };
                        match pmon.fetch(raw, tracker.last_seen_ms) {
                            Some(samples) => {
                                let m: std::collections::HashMap<
                                    u32,
                                    super::nvml_pmon::ProcUtilSample,
                                > = samples.iter().map(|s| (s.pid, *s)).collect();
                                if let Some(mx) = samples.iter().map(|s| s.ts_ms).max() {
                                    tracker.last_seen_ms = mx;
                                }
                                m
                            }
                            None => Default::default(),
                        }
                    }
                    _ => Default::default(),
                };
            let mut per_kind: Vec<GpuProc> = Vec::new();
            let compute = dev.running_compute_processes();
            for (kind, infos) in [
                (ProcKind::Compute, compute),
                (ProcKind::Graphics, dev.running_graphics_processes()),
            ] {
                if let Ok(list) = infos {
                    for p in list {
                        let (sm_pct, mem_pct, enc_pct, dec_pct) = pmon_by_pid
                            .get(&p.pid)
                            .map(|s| {
                                (
                                    Some(s.sm as f32),
                                    Some(s.mem as f32),
                                    Some(s.enc as f32),
                                    Some(s.dec as f32),
                                )
                            })
                            .unwrap_or((None, None, None, None));
                        per_kind.push(GpuProc {
                            pid: p.pid,
                            gpu_index: *i,
                            kind,
                            mem_used: match p.used_gpu_memory {
                                UsedGpuMemory::Used(v) => v,
                                UsedGpuMemory::Unavailable => 0,
                            },
                            sm_pct,
                            mem_pct,
                            enc_pct,
                            dec_pct,
                            cpu_pct: None,
                            uid: 0,
                            user: "?".into(),
                            command: String::new(),
                            start_time: 0,
                        });
                    }
                }
            }
            // merge C+G (Python parity)
            for p in per_kind {
                match out.iter_mut().find(|o| o.pid == p.pid) {
                    Some(existing) => {
                        existing.kind = ProcKind::ComputeAndGraphics;
                        existing.mem_used = existing.mem_used.max(p.mem_used);
                    }
                    None => out.push(p),
                }
            }
        }
        crate::procfs::enrich_procs(&mut out, &mut self.last_cpu.lock().unwrap());
        out
    }
}
