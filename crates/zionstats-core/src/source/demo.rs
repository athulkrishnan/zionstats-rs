//! Demo source — a port of the Python `DemoProvider` scripted arcs:
//! two synthetic GPUs over a 120 s period; GPU 0 runs a thermal-throttle
//! sequence, GPU 1 a power-capped memory-bound arc. Deterministic per
//! phase so screenshots/CI are reproducible.

use std::time::{Instant, SystemTime};

use super::Source;
use crate::types::{DeviceInfo, Environment, GpuProc, ProcKind, Sample};

pub const PERIOD_S: f64 = 120.0;

pub struct DemoSource {
    t0: Instant,
    /// seconds into the scripted arc (tests pin this; live uses wall clock)
    pub phase_override: Option<f64>,
}

impl Default for DemoSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoSource {
    pub fn new() -> Self {
        DemoSource {
            t0: Instant::now(),
            phase_override: None,
        }
    }

    fn phase(&self) -> f64 {
        if let Some(p) = self.phase_override {
            return p % PERIOD_S;
        }
        (self.t0.elapsed().as_secs_f64()) % PERIOD_S
    }

    fn jitter(&self, amount: f64) -> f64 {
        // deterministic pseudo-noise from the phase: no RNG dep needed
        let x = (self.phase() * 12.9898).fract() * 43758.5453;
        amount * (x.fract() - 0.5)
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sample_gpu0(&self, t: f64) -> Sample {
        let load: f64 = if t < 15.0 {
            3.0 + self.jitter(2.0)
        } else if t < 30.0 {
            20.0 + (t - 15.0) * 6.0 + self.jitter(4.0)
        } else if t < 70.0 {
            96.0 + self.jitter(4.0)
        } else if t < 95.0 {
            99.0 + self.jitter(2.0)
        } else {
            (99.0 - (t - 95.0) * 5.0).max(4.0) + self.jitter(3.0)
        };
        let load = load.clamp(0.0, 100.0);
        let temp = (36.0 + (t - 12.0).clamp(0.0, 41.6) * 1.25 - (t - 95.0).max(0.0) * 2.0
            + self.jitter(0.8))
        .clamp(33.0, 89.0);
        let throttling = (55.0..100.0).contains(&t);
        let sm_clock: f64 = if load > 30.0 {
            2850.0
        } else {
            240.0 + self.jitter(40.0)
        } - if throttling { 620.0 } else { 0.0 }
            + self.jitter(30.0);
        Sample {
            ts: self.now(),
            index: 0,
            uuid: "GPU-demo-4070tis".into(),
            name: "NVIDIA GeForce RTX 4070 Ti SUPER".into(),
            pstate: Some(if load > 8.0 { 0 } else { 8 }),
            util_gpu: Some(load as f32),
            util_mem: Some((load * 0.82 + self.jitter(6.0)).clamp(0.0, 100.0) as f32),
            util_enc: Some(if (20.0..65.0).contains(&t) {
                ((40.0 - (t - 42.0).abs() * 2.0).max(0.0)) as f32
            } else {
                0.0
            }),
            util_dec: Some(0.0),
            mem_used: 1_100_000_000u64
                + if t > 18.0 {
                    (13.2e9_f64 * ((t - 15.0) / 3.0).clamp(0.0, 1.0)) as u64
                } else {
                    0
                },
            mem_total: 16 << 30,
            power_w: Some(
                (28.0 + load * 2.62 - if throttling { 60.0 } else { 0.0 }).max(14.0) as f32,
            ),
            power_limit_w: Some(285.0),
            clocks_sm_mhz: Some(sm_clock as u32),
            clocks_mem_mhz: Some(10951),
            clocks_max_sm_mhz: Some(2850),
            energy_mj: Some((self.phase() * 137_000.0) as u64),
            throttle_bits: if throttling {
                0x020
            } else if load < 3.0 {
                0x001
            } else {
                0
            },
            throttle: if throttling {
                vec!["SW-Thermal".into()]
            } else if load < 3.0 {
                vec!["GPU-Idle".into()]
            } else {
                vec![]
            },
            temp_gpu: Some(temp as f32),
            temp_mem: Some(54.0),
            temp_slowdown: Some(90.0),
            temp_shutdown: Some(98.0),
            fan_pct: Some((((temp - 30.0) * 1.7).clamp(22.0, 100.0)) as f32),
            pcie_gen_cur: Some(4),
            pcie_gen_max: Some(4),
            pcie_width_cur: Some(16),
            pcie_width_max: Some(16),
            pcie_rx_kbps: Some(((load - 20.0).max(0.0) * 18_000.0) as u32),
            pcie_tx_kbps: Some(((load - 20.0).max(0.0) * 4_000.0) as u32),
            ecc_sbe: Some(0),
            ecc_dbe: Some(0),
        }
    }

    fn sample_gpu1(&self, t: f64) -> Sample {
        let (l1, m1): (f64, f64) = if t < 25.0 {
            (8.0 + self.jitter(3.0), 12.0 + self.jitter(4.0))
        } else if t < 80.0 {
            (88.0 + self.jitter(5.0), (40.0 + (t - 25.0) * 2.2).min(99.0))
        } else {
            (88.0 + self.jitter(5.0), 99.0 + self.jitter(1.0))
        };
        Sample {
            ts: self.now(),
            index: 1,
            uuid: "GPU-demo-h100".into(),
            name: "NVIDIA H100 PCIe".into(),
            pstate: Some(if l1 > 8.0 { 0 } else { 2 }),
            util_gpu: Some(l1 as f32),
            util_mem: Some(m1 as f32),
            util_enc: Some(0.0),
            util_dec: Some(if (40.0..90.0).contains(&t) {
                ((30.0 - (t - 60.0).abs()).max(0.0)) as f32
            } else {
                0.0
            }),
            mem_used: (if t < 25.0 { 18 } else { 74 }) << 30,
            mem_total: 80 << 30,
            power_w: Some(if t > 20.0 {
                ((45.0 + l1 * 3.3).min(348.0)) as f32
            } else {
                43.0
            }),
            power_limit_w: Some(350.0),
            clocks_sm_mhz: Some(if l1 > 30.0 { 1980 } else { 345 }),
            clocks_mem_mhz: Some(2619),
            clocks_max_sm_mhz: Some(1980),
            energy_mj: Some((self.phase() * 210_000.0) as u64),
            throttle_bits: if l1 > 60.0 {
                0x004
            } else if l1 < 3.0 {
                0x001
            } else {
                0
            },
            throttle: if l1 > 60.0 {
                vec!["SW-Power-Cap".into()]
            } else if l1 < 3.0 {
                vec!["GPU-Idle".into()]
            } else {
                vec![]
            },
            temp_gpu: Some((44.0 + (t - 20.0).clamp(0.0, 69.0) * 0.45) as f32),
            temp_mem: Some(52.0),
            temp_slowdown: Some(90.0),
            temp_shutdown: Some(95.0),
            fan_pct: Some((38.0 + (t - 20.0).max(0.0) * 0.6) as f32),
            pcie_gen_cur: Some(5),
            pcie_gen_max: Some(5),
            pcie_width_cur: Some(16),
            pcie_width_max: Some(16),
            pcie_rx_kbps: Some(if l1 > 10.0 {
                ((l1 - 10.0) * 26_000.0) as u32
            } else {
                120
            }),
            pcie_tx_kbps: Some(if l1 > 10.0 {
                ((l1 - 10.0) * 5_000.0) as u32
            } else {
                90
            }),
            ecc_sbe: Some(2),
            ecc_dbe: Some(0),
        }
    }
}

impl Source for DemoSource {
    fn environment(&mut self) -> Environment {
        Environment {
            hostname: "zion-demo".into(),
            kernel: "Linux 6.9.0 (demo)".into(),
            driver: "580.00 (demo)".into(),
            nvml: "13.590 (demo)".into(),
            cuda_max: "13.0".into(),
            provider: "demo",
        }
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, super::SourceError> {
        Ok(vec![
            DeviceInfo {
                index: 0,
                name: "NVIDIA GeForce RTX 4070 Ti SUPER".into(),
                uuid: "GPU-demo-4070tis".into(),
            },
            DeviceInfo {
                index: 1,
                name: "NVIDIA H100 PCIe".into(),
                uuid: "GPU-demo-h100".into(),
            },
        ])
    }

    fn sample(&mut self) -> Vec<Sample> {
        let t = self.phase();
        vec![self.sample_gpu0(t), self.sample_gpu1(t)]
    }

    fn processes(&mut self) -> Vec<GpuProc> {
        let t = self.phase();
        let mut procs = vec![
            GpuProc {
                pid: 184722,
                gpu_index: 0,
                kind: ProcKind::Compute,
                mem_used: (13.4 * (1 << 30) as f64) as u64,
                sm_pct: Some(97.0),
                mem_pct: Some(84.0),
                enc_pct: Some(0.0),
                dec_pct: Some(0.0),
                cpu_pct: Some(312.0),
                uid: 1000,
                user: "demo".into(),
                command: "python train.py --batch 256 --amp".into(),
                start_time: 100,
            },
            GpuProc {
                pid: 184701,
                gpu_index: 0,
                kind: ProcKind::Graphics,
                mem_used: 1180 << 20,
                sm_pct: Some(2.0),
                mem_pct: Some(3.0),
                enc_pct: Some(0.0),
                dec_pct: Some(0.0),
                cpu_pct: Some(4.1),
                uid: 1000,
                user: "demo".into(),
                command: "chrome --type=gpu-process".into(),
                start_time: 101,
            },
        ];
        if t > 25.0 {
            procs.push(GpuProc {
                pid: 192004,
                gpu_index: 1,
                kind: ProcKind::Compute,
                mem_used: 73 << 30,
                sm_pct: Some(88.0),
                mem_pct: Some(95.0),
                enc_pct: Some(0.0),
                dec_pct: Some(12.0),
                cpu_pct: Some(26.0),
                uid: 1001,
                user: "mlteam".into(),
                command: "python -m vllm.entrypoints serve".into(),
                start_time: 102,
            });
            procs.push(GpuProc {
                pid: 192101,
                gpu_index: 1,
                kind: ProcKind::Compute,
                mem_used: 2 << 30,
                sm_pct: Some(6.0),
                mem_pct: Some(2.0),
                enc_pct: Some(0.0),
                dec_pct: Some(0.0),
                cpu_pct: Some(1.2),
                uid: 1001,
                user: "mlteam".into(),
                command: "python watchdog.py".into(),
                start_time: 103,
            });
        }
        if t <= 15.0 {
            procs.retain(|p| !(p.gpu_index == 0 && p.kind == ProcKind::Compute));
        }
        procs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_arcs_are_deterministic_per_phase() {
        let mut d = DemoSource::new();
        d.phase_override = Some(45.0);
        let s = d.sample();
        assert_eq!(s.len(), 2);
        assert!(
            s[0].util_gpu.unwrap() > 90.0,
            "t=45 is the sustained-load phase"
        );
        assert_eq!(
            s[0].throttle,
            Vec::<String>::new(),
            "t=45 is sustained but pre-throttle (throttle begins t=55)"
        );
        assert!(s[1].throttle.contains(&"SW-Power-Cap".to_string()));

        d.phase_override = Some(60.0);
        let throttled = d.sample();
        assert_eq!(
            throttled[0].throttle,
            vec!["SW-Thermal"],
            "t=60 is inside the throttle arc"
        );

        d.phase_override = Some(5.0);
        let idle = d.sample();
        assert!(idle[0].util_gpu.unwrap() < 10.0, "t=5 is idle");
        assert!(
            idle[0].throttle.is_empty() || idle[0].throttle == vec!["GPU-Idle"],
            "idle phase may or may not cross the 3% idle-bit threshold"
        );
    }

    #[test]
    fn demo_processes_phase_gate() {
        let mut d = DemoSource::new();
        d.phase_override = Some(5.0);
        assert_eq!(d.processes().len(), 1, "chrome only before t=15");
        d.phase_override = Some(20.0);
        assert_eq!(d.processes().len(), 2, "trainer appears after t=15");
        d.phase_override = Some(45.0);
        assert_eq!(d.processes().len(), 4, "vllm pair after t=25");
    }
}
