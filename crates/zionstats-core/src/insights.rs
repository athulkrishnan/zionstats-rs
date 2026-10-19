//! Insight accumulators — a faithful port of the sibling Python
//! `zionstats/insights.py`: session Wh (NVML mJ counter preferred with a
//! wrap guard, power integration fallback with the same 10 s dt clamp),
//! avg/peak/idle-leak watts, clock deficit at load, PCIe saturation vs
//! the theoretical per-lane table, and a thermal trend from a 32-sample
//! history. Pure struct with injectable-free timestamps (the sample
//! carries its own unix time, so table tests pass synthetic clocks).

use std::collections::HashMap;

use crate::types::Sample;

const TEMP_HIST_LEN: usize = 32;
const MJ_PER_WH: f64 = 3_600_000.0;
/// theoretical per-lane GB/s by PCIe generation (Python `_PCIE_GBPS`)
const PCIE_GBPS_PER_LANE: [f32; 7] = [0.0, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceInsight {
    pub session_wh: f64,
    pub avg_w: Option<f32>,
    pub peak_w: Option<f32>,
    pub idle_leak_w: Option<f32>,
    pub clock_deficit_mhz: u32,
    pub pcie_theoretical_gbps: Option<f32>,
    pub pcie_gbps: f32,
    /// measured/theoretical clamped to 0..1
    pub pcie_sat: f32,
    pub thermal_rate_c_per_min: f32,
}

#[derive(Default)]
struct DeviceState {
    energy_wh: f64,
    last_ts: Option<f64>,
    last_energy_mj: Option<u64>,
    power_sum: f64,
    power_n: u64,
    peak_w: Option<f32>,
    idle_w_sum: f64,
    idle_n: u64,
    temp_hist: std::collections::VecDeque<(f64, f32)>,
}

#[derive(Default)]
pub struct Insights {
    st: HashMap<u32, DeviceState>,
}

fn sample_unix(s: &Sample) -> f64 {
    s.ts.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Insights {
    pub fn reset(&mut self) {
        self.st.clear();
    }

    /// One sample in, the derived insight out. Never panics; missing
    /// inputs leave the corresponding output at its neutral value.
    pub fn update(&mut self, s: &Sample) -> DeviceInsight {
        let ins = DeviceInsight::default();
        let st = self.st.entry(s.index).or_default();
        let now = sample_unix(s);

        // ---- energy (mJ counter preferred, integration fallback)
        if let (Some(mj), Some(last)) = (s.energy_mj, st.last_energy_mj) {
            let d = mj.saturating_sub(last);
            if (d as f64) < 3.6e9 {
                st.energy_wh += d as f64 / MJ_PER_WH;
            }
        } else if let (Some(pw), Some(last)) = (s.power_w, st.last_ts) {
            let dt = (now - last).clamp(0.0, 10.0);
            st.energy_wh += (pw as f64) * dt / 3600.0;
        }
        if s.energy_mj.is_some() {
            st.last_energy_mj = s.energy_mj;
        }
        st.last_ts = Some(now);
        let mut ins = ins;
        ins.session_wh = st.energy_wh;

        // ---- power stats
        if let Some(pw) = s.power_w {
            st.power_sum += pw as f64;
            st.power_n += 1;
            st.peak_w = Some(st.peak_w.map_or(pw, |p: f32| p.max(pw)));
            if s.util_gpu.is_some_and(|u| u < 5.0) {
                st.idle_w_sum += pw as f64;
                st.idle_n += 1;
            }
            ins.avg_w = Some((st.power_sum / st.power_n as f64) as f32);
            ins.peak_w = st.peak_w;
            ins.idle_leak_w = (st.idle_n > 0).then(|| (st.idle_w_sum / st.idle_n as f64) as f32);
        }

        // ---- clocks (deficit only matters under load)
        if let (Some(sm), Some(max), Some(util)) =
            (s.clocks_sm_mhz, s.clocks_max_sm_mhz, s.util_gpu)
        {
            if util > 50.0 {
                ins.clock_deficit_mhz = max.saturating_sub(sm);
            }
        }

        // ---- thermal trend (32-sample history)
        if let Some(t) = s.temp_gpu {
            st.temp_hist.push_back((now, t));
            while st.temp_hist.len() > TEMP_HIST_LEN {
                st.temp_hist.pop_front();
            }
            if st.temp_hist.len() >= 4 {
                let xs: Vec<(f64, f32)> = st.temp_hist.iter().copied().collect();
                let span = (xs[xs.len() - 1].0 - xs[0].0).max(1.0);
                ins.thermal_rate_c_per_min = (xs[xs.len() - 1].1 - xs[0].1) / span as f32 * 60.0;
            }
        }

        // ---- pcie saturation
        let gen = s.pcie_gen_max.unwrap_or(4) as usize;
        let width = s.pcie_width_max.unwrap_or(16) as f32;
        let per_lane = PCIE_GBPS_PER_LANE.get(gen).copied().unwrap_or(2.0);
        ins.pcie_theoretical_gbps = Some(per_lane * width);
        let rx = s.pcie_rx_kbps.unwrap_or(0) as f32 / 1e6;
        let tx = s.pcie_tx_kbps.unwrap_or(0) as f32 / 1e6;
        ins.pcie_gbps = rx + tx;
        let theo = ins.pcie_theoretical_gbps.unwrap_or(1.0).max(1.0);
        ins.pcie_sat = (ins.pcie_gbps / theo).clamp(0.0, 1.0);
        ins
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn sample_at(index: u32, unix: f64) -> Sample {
        Sample {
            ts: UNIX_EPOCH + Duration::from_secs_f64(unix),
            index,
            ..Default::default()
        }
    }

    #[test]
    fn energy_mj_counter_preferred_with_wrap_guard() {
        let mut eng = Insights::default();
        let mut s = sample_at(0, 0.0);
        s.energy_mj = Some(1_000_000);
        eng.update(&s);
        s.ts = UNIX_EPOCH + Duration::from_secs_f64(10.0);
        s.energy_mj = Some(4_600_000); // +3.6e6 mJ = 1 Wh
        let ins = eng.update(&s);
        assert!((ins.session_wh - 1.0).abs() < 1e-9, "mJ delta wins");
        // wrap/reset: delta beyond the guard is ignored, not accumulated
        s.energy_mj = Some(9_000_000_000u64);
        let ins = eng.update(&s);
        assert!((ins.session_wh - 1.0).abs() < 1e-9, "wrap guard holds");
    }

    #[test]
    fn power_integration_fallback_and_clamp() {
        let mut eng = Insights::default();
        let mut s = sample_at(0, 0.0);
        s.power_w = Some(100.0);
        eng.update(&s);
        // +10 s at 100 W -> 100*10/3600 Wh (the Python dt clamp)
        s.ts = UNIX_EPOCH + Duration::from_secs_f64(10.0);
        let ins = eng.update(&s);
        assert!((ins.session_wh - 100.0 * 10.0 / 3600.0).abs() < 1e-9);
        // a 1000 s gap contributes at most 10 s worth
        s.ts = UNIX_EPOCH + Duration::from_secs_f64(1010.0);
        let ins = eng.update(&s);
        assert!(
            (ins.session_wh - 2.0 * 100.0 * 10.0 / 3600.0).abs() < 1e-9,
            "dt clamped at 10 s"
        );
    }

    #[test]
    fn power_stats_and_idle_leak() {
        let mut eng = Insights::default();
        let mut s = sample_at(0, 0.0);
        s.power_w = Some(20.0);
        s.util_gpu = Some(2.0); // idle
        eng.update(&s);
        s.ts = UNIX_EPOCH + Duration::from_secs_f64(1.0);
        s.power_w = Some(300.0);
        s.util_gpu = Some(90.0);
        let ins = eng.update(&s);
        assert_eq!(ins.peak_w, Some(300.0));
        assert_eq!(ins.avg_w, Some(160.0));
        assert_eq!(ins.idle_leak_w, Some(20.0));
    }

    #[test]
    fn clock_deficit_only_under_load() {
        let mut eng = Insights::default();
        let mut s = sample_at(0, 0.0);
        s.clocks_sm_mhz = Some(1500);
        s.clocks_max_sm_mhz = Some(2500);
        s.util_gpu = Some(30.0);
        let ins = eng.update(&s);
        assert_eq!(ins.clock_deficit_mhz, 0, "idle deficit is noise");
        s.util_gpu = Some(80.0);
        let ins = eng.update(&s);
        assert_eq!(ins.clock_deficit_mhz, 1000);
    }

    #[test]
    fn pcie_saturation_table_and_clamp() {
        let mut eng = Insights::default();
        let mut s = sample_at(0, 0.0);
        s.pcie_gen_max = Some(4);
        s.pcie_width_max = Some(16);
        s.pcie_rx_kbps = Some(2_000_000); // 2 GB/s
        s.pcie_tx_kbps = Some(2_000_000);
        let ins = eng.update(&s);
        assert_eq!(ins.pcie_theoretical_gbps, Some(32.0));
        assert!((ins.pcie_sat - 0.125).abs() < 1e-6);
        // over-theoretical input clamps to 1.0
        s.pcie_rx_kbps = Some(90_000_000);
        let ins = eng.update(&s);
        assert_eq!(ins.pcie_sat, 1.0);
    }

    #[test]
    fn thermal_rate_from_history() {
        let mut eng = Insights::default();
        for i in 0..4i64 {
            let mut s = sample_at(0, i as f64 * 60.0); // 1 min apart
            s.temp_gpu = Some(50.0 + 2.0 * i as f32);
            let ins = eng.update(&s);
            if i == 3 {
                assert!((ins.thermal_rate_c_per_min - 2.0).abs() < 1e-6);
            }
        }
    }
}
