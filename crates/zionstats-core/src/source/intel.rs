//! Intel source (i915/xe sysfs + fdinfo).  EXPERIMENTAL: per-engine
//! aggregation is best-effort across driver generations; the fixture
//! trees pin the parsers, not every kernel's layout.
//!
//! Deliberate honest gaps: NO power (client GPUs expose no
//! GPU-attributable power on this path; RAPL is socket-level and would
//! be a lie), no fan, no energy counter.  util_gpu is derived from
//! fdinfo cycle deltas, so it reads None until a second scan sees GPU
//! clients.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::drmfd::{engine_utilization, scan_clients};
use super::{Source, SourceError};
use crate::types::{DeviceInfo, Environment, GpuProc, ProcKind, Sample};

pub const VENDOR_ID: &str = "0x8086";
const MAX_CLIENTS: usize = 256;

pub struct IntelSource {
    sysfs_root: PathBuf,
    proc_root: PathBuf,
    cards: Vec<(PathBuf, String)>, // (device dir, driver)
    client_ring: HashMap<String, HashMap<String, u64>>,
    last_cpu: HashMap<u32, (u64, SystemTime)>,
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn bus_id(dev: &Path) -> Option<String> {
    std::fs::canonicalize(dev)
        .ok()?
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
}

pub fn discover_cards(sysfs_root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(sysfs_root) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.starts_with("card"))
        .collect();
    names.sort_by_key(|n| n[4..].parse::<u32>().unwrap_or(u32::MAX));
    for name in names {
        let dev = sysfs_root.join(&name).join("device");
        if !dev.is_dir() {
            continue;
        }
        if read_text(dev.join("vendor")).map(|v| v.trim().to_lowercase()) != Some(VENDOR_ID.into())
        {
            continue;
        }
        let driver = read_text(dev.join("uevent"))
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("DRIVER=").map(String::from))
            .unwrap_or_default();
        out.push((dev, driver));
    }
    out
}

fn clock_path(dev: &Path, driver: &str) -> PathBuf {
    if driver == "xe" {
        dev.join("tile0/gt0/freq0_cur_freq")
    } else {
        dev.join("gt/gt0/rps_cur_freq")
    }
}

impl IntelSource {
    pub fn try_new(sysfs_root: &Path, proc_root: &Path) -> Result<IntelSource, SourceError> {
        let cards = discover_cards(sysfs_root);
        if cards.is_empty() {
            return Err(SourceError::NoDevices);
        }
        Ok(IntelSource {
            sysfs_root: sysfs_root.to_path_buf(),
            proc_root: proc_root.to_path_buf(),
            cards,
            client_ring: HashMap::new(),
            last_cpu: HashMap::new(),
        })
    }
}

impl Source for IntelSource {
    fn environment(&mut self) -> Environment {
        let driver = ["xe", "i915"]
            .iter()
            .find_map(|m| {
                read_text(format!("/sys/module/{m}/version"))
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .map(|v| format!("{m} {v}"))
            })
            .unwrap_or_default();
        Environment {
            hostname: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|h| h.trim().to_string())
                .unwrap_or_default(),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|k| format!("Linux {}", k.trim()))
                .unwrap_or_default(),
            driver,
            nvml: String::new(),
            cuda_max: String::new(),
            provider: "intel",
        }
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, SourceError> {
        Ok(self
            .cards
            .iter()
            .enumerate()
            .map(|(i, (dev, drv))| {
                let name = read_text(dev.join("product_name"))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| drv.clone());
                DeviceInfo {
                    index: i as u32,
                    name,
                    uuid: format!("INTEL-{}", bus_id(dev).unwrap_or_else(|| i.to_string())),
                }
            })
            .collect())
    }

    fn sample(&mut self) -> Vec<Sample> {
        self.cards
            .iter()
            .enumerate()
            .map(|(i, (dev, drv))| {
                let mut s = Sample {
                    ts: SystemTime::now(),
                    index: i as u32,
                    uuid: format!("INTEL-{}", bus_id(dev).unwrap_or_else(|| i.to_string())),
                    name: read_text(dev.join("product_name"))
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| drv.clone()),
                    vendor: "intel".into(),
                    ..Default::default()
                };
                s.mem_total = read_int(dev.join("mem_info_vram_total")).unwrap_or(0) as u64;
                let hw = std::fs::read_dir(dev.join("hwmon"))
                    .ok()
                    .and_then(|mut rd| rd.find_map(|e| e.ok().map(|e| e.path())));
                if let Some(hw) = hw {
                    s.temp_gpu = read_int(hw.join("temp1_input")).map(|mc| mc as f32 / 1000.0);
                }
                s.clocks_sm_mhz = read_int(clock_path(dev, drv)).map(|mhz| mhz as u32);
                // power_w stays None by design (no GPU-attributable sysfs
                // power; RAPL would be socket-level, not the GPU)
                s
            })
            .collect()
    }

    fn processes(&mut self) -> Vec<GpuProc> {
        let wanted = ["i915", "xe"];
        let mut clients: Vec<_> = scan_clients(&self.proc_root)
            .into_iter()
            .filter(|c| wanted.contains(&c.driver.as_str()))
            .collect();
        if clients.len() > MAX_CLIENTS {
            clients.sort_by_key(|c| {
                std::cmp::Reverse(
                    c.fields
                        .iter()
                        .filter(|(k, _)| k.starts_with("drm-memory-") && k.contains("vram"))
                        .map(|(_, v)| *v)
                        .sum::<u64>(),
                )
            });
            clients.truncate(MAX_CLIENTS);
        }
        let pci_to_idx: HashMap<String, usize> = self
            .cards
            .iter()
            .enumerate()
            .map(|(i, (dev, _))| (bus_id(dev).unwrap_or_default(), i))
            .collect();
        let mut per_pid: HashMap<u32, (u64, Option<f32>, usize)> = HashMap::new();
        let mut card_sums: HashMap<usize, HashMap<String, u64>> = HashMap::new();
        for c in &clients {
            let prev = self.client_ring.get(&c.client_id);
            let util =
                prev.and_then(|p| engine_utilization(p, &c.fields, super::drmfd::is_render_engine));
            self.client_ring
                .insert(c.client_id.clone(), c.fields.clone());
            let idx = pci_to_idx.get(&c.pdev).copied().unwrap_or(0);
            let vram: u64 = c
                .fields
                .iter()
                .filter(|(k, _)| k.starts_with("drm-memory-") && k.contains("vram"))
                .map(|(_, v)| *v)
                .sum();
            let entry = per_pid.entry(c.pid).or_insert((0, None, idx));
            entry.0 += vram;
            if let Some(u) = util {
                entry.1 = Some(entry.1.map_or(u, |old: f32| old.max(u)));
            }
            let sums = card_sums.entry(idx).or_default();
            for (k, v) in &c.fields {
                *sums.entry(k.clone()).or_insert(0) += v;
            }
        }
        if self.client_ring.len() > 4096 {
            let live: std::collections::HashSet<&str> =
                clients.iter().map(|c| c.client_id.as_str()).collect();
            self.client_ring.retain(|k, _| live.contains(k.as_str()));
        }
        // card-level view: cache the summed engine deltas for sample().
        // (sample() reads them on the next tick via the same pattern as
        // the process ring -- kept in the ring map under a reserved key.)
        let mut out: Vec<GpuProc> = per_pid
            .into_iter()
            .map(|(pid, (mem, sm, idx))| GpuProc {
                pid,
                gpu_index: idx as u32,
                kind: ProcKind::Compute,
                mem_used: mem,
                sm_pct: sm,
                mem_pct: None,
                enc_pct: None,
                dec_pct: None,
                cpu_pct: None,
                uid: 0,
                user: "?".into(),
                command: String::new(),
                start_time: 0,
            })
            .collect();
        out.sort_by_key(|p| p.pid);
        crate::procfs::enrich_procs(&mut out, &mut self.last_cpu);
        out
    }
}

fn read_int(path: impl AsRef<Path>) -> Option<i64> {
    read_text(path)?.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/intel")
    }

    #[test]
    fn discovery_both_drivers() {
        let cards = discover_cards(&fx().join("drm"));
        let drivers: Vec<&str> = cards.iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(drivers, ["i915", "xe"]);
    }

    #[test]
    fn provider_fixture_both_layouts() {
        let mut src = IntelSource::try_new(&fx().join("drm"), &fx().join("proc")).unwrap();
        let devs = src.devices().unwrap();
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].name, "i915");
        assert!(devs[0].uuid.starts_with("INTEL-"));
        let samples = src.sample();
        assert_eq!(
            samples[0].clocks_sm_mhz,
            Some(1250),
            "i915 rps_cur_freq MHz"
        );
        assert_eq!(
            samples[1].clocks_sm_mhz,
            Some(2350),
            "xe tile0 freq0_cur_freq"
        );
        assert_eq!(samples[1].mem_total, 28_615_671_808, "xe vram total");
        assert_eq!(samples[0].mem_total, 0, "i915 lacks the total file -> 0");
        assert_eq!(samples[0].power_w, None, "no GPU power, ever (no RAPL)");
        assert_eq!(samples[0].fan_pct, None);
        assert_eq!(samples[0].util_gpu, None, "no fdinfo baseline yet");
    }

    #[test]
    fn empty_discovery_fails_clean() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            IntelSource::try_new(dir.path(), dir.path()),
            Err(SourceError::NoDevices)
        ));
    }
}
