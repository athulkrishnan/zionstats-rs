//! Per-process GPU utilization ("pmon") via the raw
//! `nvmlDeviceGetProcessUtilization` FFI. The binding already exists in
//! nvml-wrapper-sys (verified: `nvmlProcessUtilizationSample_t { pid,
//! timeStamp, smUtil, memUtil, encUtil, decUtil }` and the two-call
//! count/fetch contract), so no hand-written extern block is needed; the
//! raw device handle comes from the wrapper's `Device::handle()`.
//!
//! The FFI call sits behind a trait so the merge/timestamp logic is
//! testable GPU-free; utilization values are percentages already
//! (Python `_pid_utils` parity), and processes absent from a sample
//! keep `None`.

use nvml_wrapper_sys::bindings::{
    nvmlReturn_enum_NVML_ERROR_INSUFFICIENT_SIZE, nvmlReturn_enum_NVML_SUCCESS, NvmlLib,
};

use crate::types::GpuProc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcUtilSample {
    pub pid: u32,
    pub ts_ms: u64,
    pub sm: u32,
    pub mem: u32,
    pub enc: u32,
    pub dec: u32,
}

/// FFI seam: real driver on one side, fixture vectors in tests.
pub trait PmonSource {
    fn fetch(
        &mut self,
        device: nvml_wrapper_sys::bindings::nvmlDevice_t,
        last_seen_ms: u64,
    ) -> Option<Vec<ProcUtilSample>>;
}

/// Real driver binding. `NvmlLib::new` dlopens libnvidia-ml independently
/// of the wrapper's instance; NVML state is process-global, so this is
/// safe alongside `Nvml`. The raw fn FIELD is used (not the panicking
/// helper method): an old driver without the symbol makes the feature
/// simply absent, never a crash.
pub struct NvmlPmonFfi {
    syms: NvmlLib,
}

impl NvmlPmonFfi {
    pub fn try_new() -> Option<NvmlPmonFfi> {
        let syms = unsafe { NvmlLib::new("libnvidia-ml.so.1") }.ok()?;
        if syms.nvmlDeviceGetProcessUtilization.is_err() {
            return None;
        }
        Some(NvmlPmonFfi { syms })
    }
}

impl PmonSource for NvmlPmonFfi {
    fn fetch(
        &mut self,
        device: nvml_wrapper_sys::bindings::nvmlDevice_t,
        last_seen_ms: u64,
    ) -> Option<Vec<ProcUtilSample>> {
        let get_process_util = self.syms.nvmlDeviceGetProcessUtilization.as_ref().ok()?;
        unsafe {
            let mut count: libc::c_uint = 0;
            // two-call pattern: an empty buffer returns the needed count
            let rc = get_process_util(device, std::ptr::null_mut(), &mut count, last_seen_ms);
            if rc == nvmlReturn_enum_NVML_SUCCESS && count == 0 {
                return Some(Vec::new());
            }
            if rc != nvmlReturn_enum_NVML_ERROR_INSUFFICIENT_SIZE {
                return None;
            }
            if count == 0 || count > 4096 {
                return Some(Vec::new());
            }
            let mut buf: Vec<nvml_wrapper_sys::bindings::nvmlProcessUtilizationSample_t> = vec![
                    nvml_wrapper_sys::bindings::nvmlProcessUtilizationSample_t {
                        pid: 0,
                        timeStamp: 0,
                        smUtil: 0,
                        memUtil: 0,
                        encUtil: 0,
                        decUtil: 0,
                    };
                    count as usize
                ];
            let mut got = count;
            let rc = get_process_util(device, buf.as_mut_ptr(), &mut got, last_seen_ms);
            if rc != nvmlReturn_enum_NVML_SUCCESS {
                return None;
            }
            buf.truncate(got as usize);
            Some(
                buf.into_iter()
                    .map(|s| ProcUtilSample {
                        pid: s.pid,
                        ts_ms: s.timeStamp,
                        sm: s.smUtil,
                        mem: s.memUtil,
                        enc: s.encUtil,
                        dec: s.decUtil,
                    })
                    .collect(),
            )
        }
    }
}

/// Per-device tracker (Python `_last_seen_util_ts` parity): keep the max
/// timestamp seen so the next call only reports deltas since then.
#[derive(Default)]
pub struct PmonTracker {
    pub last_seen_ms: u64,
}

/// Pure merge: fill the four util slots by pid (a pid's multiple samples
/// resolve to the newest), update the tracker; processes absent from the
/// sample keep None.
pub fn merge_into_procs(
    procs: &mut [GpuProc],
    samples: &[ProcUtilSample],
    tracker: &mut PmonTracker,
) {
    for p in procs.iter_mut() {
        if let Some(s) = samples
            .iter()
            .filter(|s| s.pid == p.pid)
            .max_by_key(|s| s.ts_ms)
        {
            p.sm_pct = Some(s.sm as f32);
            p.mem_pct = Some(s.mem as f32);
            p.enc_pct = Some(s.enc as f32);
            p.dec_pct = Some(s.dec as f32);
        }
    }
    if let Some(mx) = samples.iter().map(|s| s.ts_ms).max() {
        tracker.last_seen_ms = mx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32) -> GpuProc {
        GpuProc {
            pid,
            gpu_index: 0,
            kind: crate::types::ProcKind::Compute,
            mem_used: 0,
            sm_pct: None,
            mem_pct: None,
            enc_pct: None,
            dec_pct: None,
            cpu_pct: None,
            uid: 0,
            user: "?".into(),
            command: String::new(),
            start_time: 0,
        }
    }

    #[test]
    fn merge_fills_by_pid() {
        let mut procs = vec![proc(100), proc(200)];
        let samples = vec![
            ProcUtilSample {
                pid: 100,
                ts_ms: 5,
                sm: 97,
                mem: 84,
                enc: 0,
                dec: 12,
            },
            ProcUtilSample {
                pid: 300,
                ts_ms: 5,
                sm: 1,
                mem: 1,
                enc: 0,
                dec: 0,
            },
        ];
        let mut tracker = PmonTracker::default();
        merge_into_procs(&mut procs, &samples, &mut tracker);
        let a = &procs[0];
        assert_eq!(
            (a.sm_pct, a.mem_pct, a.enc_pct, a.dec_pct),
            (Some(97.0), Some(84.0), Some(0.0), Some(12.0))
        );
        assert_eq!(
            (procs[1].sm_pct, procs[1].mem_pct),
            (None, None),
            "absent pid keeps None"
        );
        assert_eq!(tracker.last_seen_ms, 5);
    }

    #[test]
    fn newest_sample_wins_and_empty_is_noop() {
        let mut procs = vec![proc(100)];
        let mut tracker = PmonTracker::default();
        merge_into_procs(
            &mut procs,
            &[ProcUtilSample {
                pid: 100,
                ts_ms: 7,
                sm: 10,
                mem: 0,
                enc: 0,
                dec: 0,
            }],
            &mut tracker,
        );
        merge_into_procs(
            &mut procs,
            &[
                ProcUtilSample {
                    pid: 100,
                    ts_ms: 9,
                    sm: 42,
                    mem: 1,
                    enc: 0,
                    dec: 0,
                },
                ProcUtilSample {
                    pid: 100,
                    ts_ms: 8,
                    sm: 99,
                    mem: 0,
                    enc: 0,
                    dec: 0,
                },
            ],
            &mut tracker,
        );
        assert_eq!(procs[0].sm_pct, Some(42.0), "max timestamp wins");
        assert_eq!(tracker.last_seen_ms, 9);
        merge_into_procs(&mut procs, &[], &mut tracker);
        assert_eq!(procs[0].sm_pct, Some(42.0), "empty sample leaves values");
        assert_eq!(tracker.last_seen_ms, 9);
    }
}
