//! AMD source: amdgpu sysfs + DRM fdinfo per-process stats.  Same kernel
//! interfaces, paths, and unit conventions as the sibling Python
//! `zionstats/amd.py` (verified against the same fixture trees).
//! Best-effort everywhere: a missing or garbage sysfs file means None
//! and the field stays absent.  Honest gaps: no memory-bandwidth
//! utilization, no hw energy counter (Insights' power-integration
//! fallback covers session Wh), no PCIe telemetry, no throttle bitmask.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::drmfd::{engine_utilization, scan_clients};
use super::{Source, SourceError};
use crate::types::{DeviceInfo, Environment, GpuProc, ProcKind, Sample};

pub const VENDOR_ID: &str = "0x1002";
const MAX_CLIENTS: usize = 256;

pub struct AmdSource {
    sysfs_root: PathBuf,
    proc_root: PathBuf,
    cards: Vec<PathBuf>,
    client_ring: HashMap<String, HashMap<String, u64>>,
    last_cpu: HashMap<u32, (u64, SystemTime)>,
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_int(path: impl AsRef<Path>) -> Option<i64> {
    read_text(path)?.trim().parse::<i64>().ok()
}

// ------------------------------------------------------------- pure parsers

pub fn parse_gpu_busy_percent(text: Option<&str>) -> Option<f32> {
    let v: i64 = text?.trim().parse().ok()?;
    (0..=100).contains(&v).then_some(v as f32)
}

pub fn parse_vram(used: Option<i64>, total: Option<i64>) -> (u64, u64) {
    (
        used.filter(|v| *v >= 0).unwrap_or(0) as u64,
        total.filter(|v| *v >= 0).unwrap_or(0) as u64,
    )
}

pub fn parse_power_micro(
    avg: Option<i64>,
    cap: Option<i64>,
    min: Option<i64>,
    max: Option<i64>,
) -> (Option<f32>, Option<f32>, Option<f32>, Option<f32>) {
    fn w(v: Option<i64>) -> Option<f32> {
        v.filter(|x| *x >= 0).map(|x| x as f32 / 1_000_000.0)
    }
    (w(avg), w(cap), w(min), w(max))
}

pub fn parse_temp_milli(text: Option<i64>) -> Option<f32> {
    text.map(|mc| mc as f32 / 1000.0)
}

pub fn parse_fan_pwm(pwm: Option<i64>, max: Option<i64>) -> Option<f32> {
    let pwm = pwm?;
    let max = max.unwrap_or(255).max(1);
    if pwm < 0 {
        return None;
    }
    Some((pwm as f32 / max as f32 * 100.0).round().clamp(0.0, 100.0))
}

/// Current entry of a pp_dpm_sclk-style table: the frequency on the line
/// marked `*` (lines look like `2: 2400M *`).
pub fn parse_dpm_marked_mhz(text: Option<&str>) -> Option<u32> {
    for line in text?.lines() {
        if !line.contains('*') {
            continue;
        }
        let after = line.split_once(':').map(|x| x.1).unwrap_or(line);
        for tok in after.split_whitespace() {
            if let Some(mhz) = tok.strip_suffix('M') {
                if let Ok(v) = mhz.parse::<u32>() {
                    return Some(v);
                }
            }
        }
    }
    None
}

pub fn parse_clocks(
    freq1_hz: Option<i64>,
    freq2_hz: Option<i64>,
    sclk_table: Option<&str>,
) -> (Option<u32>, Option<u32>) {
    let sm = freq1_hz
        .filter(|v| *v > 0)
        .map(|hz| (hz as f64 / 1e6).round() as u32)
        .or_else(|| parse_dpm_marked_mhz(sclk_table));
    let mem = freq2_hz
        .filter(|v| *v > 0)
        .map(|hz| (hz as f64 / 1e6).round() as u32);
    (sm, mem)
}

// ---------------------------------------------------------------- discovery

pub fn discover_cards(sysfs_root: &Path) -> Vec<PathBuf> {
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
        if read_text(dev.join("vendor")).map(|v| v.trim().to_lowercase()) == Some(VENDOR_ID.into())
        {
            out.push(dev);
        }
    }
    out
}

// ----------------------------------------------------------------- source

impl AmdSource {
    pub fn try_new(sysfs_root: &Path, proc_root: &Path) -> Result<AmdSource, SourceError> {
        let cards = discover_cards(sysfs_root);
        if cards.is_empty() {
            return Err(SourceError::NoDevices);
        }
        Ok(AmdSource {
            sysfs_root: sysfs_root.to_path_buf(),
            proc_root: proc_root.to_path_buf(),
            cards,
            client_ring: HashMap::new(),
            last_cpu: HashMap::new(),
        })
    }

    fn card_name(&self, dev: &Path, index: usize) -> String {
        read_text(dev.join("product_name"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("AMD GPU {index}"))
    }

    fn card_uuid(&self, dev: &Path, index: usize) -> String {
        read_text(dev.join("unique_id"))
            .map(|s| format!("AMD-{}", s.trim()))
            .filter(|s| s.len() > 4)
            .unwrap_or_else(|| format!("AMD-{}", bus_id(dev).unwrap_or_else(|| index.to_string())))
    }

    fn hwmon_dir(&self, dev: &Path) -> Option<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dev.join("hwmon"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        entries.into_iter().next()
    }
}

fn bus_id(dev: &Path) -> Option<String> {
    std::fs::canonicalize(dev)
        .ok()?
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
}

impl Source for AmdSource {
    fn environment(&mut self) -> Environment {
        Environment {
            hostname: hostname_of(),
            kernel: kernel_of(),
            driver: read_text("/sys/module/amdgpu/version")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "amdgpu".into()),
            nvml: String::new(),
            cuda_max: String::new(),
            provider: "amd",
        }
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, SourceError> {
        Ok(self
            .cards
            .iter()
            .enumerate()
            .map(|(i, dev)| DeviceInfo {
                index: i as u32,
                name: self.card_name(dev, i),
                uuid: self.card_uuid(dev, i),
            })
            .collect())
    }

    fn sample(&mut self) -> Vec<Sample> {
        self.cards
            .iter()
            .enumerate()
            .map(|(i, dev)| {
                let mut s = Sample {
                    ts: SystemTime::now(),
                    index: i as u32,
                    uuid: self.card_uuid(dev, i),
                    name: self.card_name(dev, i),
                    vendor: "amd".into(),
                    ..Default::default()
                };
                s.util_gpu =
                    parse_gpu_busy_percent(read_text(dev.join("gpu_busy_percent")).as_deref());
                let (used, total) = parse_vram(
                    read_int(dev.join("mem_info_vram_used")),
                    read_int(dev.join("mem_info_vram_total")),
                );
                s.mem_used = used;
                s.mem_total = total;
                let (pw, pl, _pmin, _pmax) = parse_power_micro(
                    read_int(dev.join("power1_average")),
                    read_int(dev.join("power1_cap")),
                    read_int(dev.join("power1_cap_min")),
                    read_int(dev.join("power1_cap_max")),
                );
                s.power_w = pw;
                s.power_limit_w = pl;
                if let Some(hw) = self.hwmon_dir(dev) {
                    s.temp_gpu = parse_temp_milli(read_int(hw.join("temp1_input")));
                    s.temp_gpu_max = parse_temp_milli(read_int(hw.join("temp2_input")));
                    s.temp_mem = parse_temp_milli(read_int(hw.join("temp3_input")));
                    s.fan_pct =
                        parse_fan_pwm(read_int(hw.join("pwm1")), read_int(hw.join("pwm1_max")));
                }
                let sclk_table = read_text(dev.join("pp_dpm_sclk"));
                let (sm, mem) = parse_clocks(
                    self.hwmon_dir(dev)
                        .and_then(|hw| read_int(hw.join("freq1_input"))),
                    self.hwmon_dir(dev)
                        .and_then(|hw| read_int(hw.join("freq2_input"))),
                    sclk_table.as_deref(),
                );
                s.clocks_sm_mhz = sm;
                s.clocks_mem_mhz = mem;
                s.pstate = read_text(dev.join("power_dpm_force_performance_level"))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .map(|_| 0i8);
                s
            })
            .collect()
    }

    fn processes(&mut self) -> Vec<GpuProc> {
        let mut clients: Vec<_> = scan_clients(&self.proc_root)
            .into_iter()
            .filter(|c| c.driver == "amdgpu")
            .collect();
        if clients.len() > MAX_CLIENTS {
            clients.sort_by_key(|c| {
                std::cmp::Reverse(c.fields.get("drm-memory-vram").copied().unwrap_or(0))
            });
            clients.truncate(MAX_CLIENTS);
        }
        let pci_to_idx: HashMap<String, usize> = self
            .cards
            .iter()
            .enumerate()
            .map(|(i, dev)| (bus_id(dev).unwrap_or_default(), i))
            .collect();
        // per-pid aggregation over clients
        let mut per_pid: HashMap<u32, (u64, Option<f32>)> = HashMap::new();
        for c in &clients {
            let prev = self.client_ring.get(&c.client_id);
            let util =
                prev.and_then(|p| engine_utilization(p, &c.fields, super::drmfd::is_render_engine));
            self.client_ring
                .insert(c.client_id.clone(), c.fields.clone());
            let entry = per_pid.entry(c.pid).or_insert((0, None));
            entry.0 += c.fields.get("drm-memory-vram").copied().unwrap_or(0);
            if let Some(u) = util {
                entry.1 = Some(entry.1.map_or(u, |old: f32| old.max(u)));
            }
        }
        // bound the ring
        if self.client_ring.len() > 4096 {
            let live: std::collections::HashSet<&str> =
                clients.iter().map(|c| c.client_id.as_str()).collect();
            self.client_ring.retain(|k, _| live.contains(k.as_str()));
        }
        let mut out: Vec<GpuProc> = per_pid
            .into_iter()
            .map(|(pid, (mem, sm))| {
                let idx = clients
                    .iter()
                    .find(|c| c.pid == pid)
                    .and_then(|c| pci_to_idx.get(&c.pdev).copied())
                    .unwrap_or(0);
                GpuProc {
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
                }
            })
            .collect();
        out.sort_by_key(|p| p.pid);
        crate::procfs::enrich_procs(&mut out, &mut self.last_cpu);
        out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fx() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/amd")
    }

    #[test]
    fn parsers_units_and_garbage() {
        assert_eq!(parse_gpu_busy_percent(Some("37\n")), Some(37.0));
        assert_eq!(parse_gpu_busy_percent(Some("banana")), None);
        assert_eq!(parse_gpu_busy_percent(Some("140")), None);
        assert_eq!(parse_vram(Some(123), Some(456)), (123, 456));
        assert_eq!(parse_vram(None, Some(-5)), (0, 0));
        let (pw, pl, pmin, pmax) =
            parse_power_micro(Some(84123456), Some(254000000), Some(0), Some(299000000));
        assert!((pw.unwrap() - 84.123456).abs() < 1e-4, "uW -> W");
        assert_eq!(pl, Some(254.0));
        assert_eq!(pmin, Some(0.0));
        assert_eq!(pmax, Some(299.0));
        assert_eq!(parse_temp_milli(Some(55500)), Some(55.5));
        assert_eq!(parse_temp_milli(None), None);
        assert_eq!(parse_fan_pwm(Some(153), Some(255)), Some(60.0));
        assert_eq!(parse_fan_pwm(Some(255), None), Some(100.0));
        assert_eq!(parse_fan_pwm(None, Some(255)), None);
        assert_eq!(
            parse_clocks(Some(1_800_000_000), Some(2_500_000_000), None),
            (Some(1800), Some(2500))
        );
        assert_eq!(
            parse_clocks(
                None,
                Some(2_500_000_000),
                Some("0: 500M\n1: 2300M\n2: 2400M *\n")
            ),
            (Some(2400), Some(2500)),
            "dpm fallback when freq1 missing"
        );
        assert_eq!(
            parse_dpm_marked_mhz(Some("0: 500M\n1: 2300M *\n")),
            Some(2300)
        );
    }

    #[test]
    fn provider_over_fixture_tree() {
        let mut src = AmdSource::try_new(&fx().join("drm"), &fx().join("proc")).unwrap();
        let devs = src.devices().unwrap();
        assert_eq!(devs.len(), 1, "nvidia-vendor card1 skipped");
        assert_eq!(devs[0].name, "AMD Radeon RX 7900 XTX");
        assert!(devs[0].uuid.starts_with("AMD-"));
        let s = &src.sample()[0];
        assert_eq!(s.vendor, "amd");
        assert_eq!(s.util_gpu, Some(37.0));
        assert_eq!(s.mem_used, 1234567890);
        assert!((s.power_w.unwrap() - 84.123456).abs() < 1e-4);
        assert_eq!(s.power_limit_w, Some(254.0));
        assert_eq!(s.temp_gpu, Some(55.5));
        assert_eq!(s.temp_gpu_max, Some(78.0), "junction -> hottest part");
        assert_eq!(s.fan_pct, Some(60.0));
        assert_eq!(s.clocks_sm_mhz, Some(2400), "no freq1 -> dpm fallback");
        assert_eq!(s.clocks_mem_mhz, Some(2500));
        assert_eq!(s.energy_mj, None, "no hw counter on this path");
    }

    #[test]
    fn empty_discovery_fails_clean() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            AmdSource::try_new(dir.path(), dir.path()),
            Err(SourceError::NoDevices)
        ));
    }

    #[test]
    fn fdinfo_deltas_over_two_scans() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_dst = tmp.path().join("proc");
        copy_dir(&fx().join("proc"), &proc_dst);
        let mut src = AmdSource::try_new(&fx().join("drm"), &proc_dst).unwrap();
        let first = src.processes();
        let p = first.iter().find(|p| p.pid == 4242).unwrap();
        assert_eq!(p.mem_used, 4096 + 2048, "client memory sums");
        assert_eq!(p.sm_pct, None, "first sighting: no baseline");
        // advance counters (fd3: +1000/+10000, fd4: +300/+1000)
        let f3 = proc_dst.join("4242/fdinfo/3");
        std::fs::write(
            &f3,
            std::fs::read_to_string(&f3)
                .unwrap()
                .replace("drm-cycles-gfx:\t1000", "drm-cycles-gfx:\t2000")
                .replace(
                    "drm-total-cycles-gfx:\t10000",
                    "drm-total-cycles-gfx:\t20000",
                ),
        )
        .unwrap();
        let f4 = proc_dst.join("4242/fdinfo/4");
        std::fs::write(
            &f4,
            std::fs::read_to_string(&f4)
                .unwrap()
                .replace("drm-cycles-gfx:\t100", "drm-cycles-gfx:\t400")
                .replace("drm-total-cycles-gfx:\t1000", "drm-total-cycles-gfx:\t2000"),
        )
        .unwrap();
        let second = src.processes();
        let p = second.iter().find(|p| p.pid == 4242).unwrap();
        assert_eq!(
            p.sm_pct.map(|v| (v * 100.0).round() / 100.0),
            Some(30.0),
            "max(10%, 30%) = 30% (got {:?})",
            p.sm_pct
        );
    }

    fn copy_dir(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &dst.join(e.file_name()));
            } else if e.file_type().unwrap().is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(
                    std::fs::read_link(e.path()).unwrap(),
                    dst.join(e.file_name()),
                )
                .unwrap();
            } else {
                std::fs::copy(e.path(), dst.join(e.file_name())).unwrap();
            }
        }
    }
}
