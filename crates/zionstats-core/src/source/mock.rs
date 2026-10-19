//! Scripted source for tests/CI: deterministic, no time dependence beyond
//! an explicit timeline the test drives.

use crate::types::{DeviceInfo, Environment, GpuProc, ProcKind, Sample};
use std::time::SystemTime;

use super::{Source, SourceError};

#[derive(Default)]
pub struct MockSource {
    pub env: Option<Environment>,
    pub devices: Vec<DeviceInfo>,
    pub samples: Vec<Sample>,
    pub procs: Vec<GpuProc>,
    /// bumped by `set_sample_ts` helpers; tests assert tick propagation
    pub ticks: u64,
}

impl MockSource {
    pub fn two_gpu_loaded() -> MockSource {
        let mut s = MockSource {
            env: Some(Environment {
                hostname: "mock-box".into(),
                kernel: "Linux 6.9 (mock)".into(),
                driver: "580.00 (mock)".into(),
                nvml: "13.0 (mock)".into(),
                cuda_max: "13.0".into(),
                provider: "mock",
            }),
            ..Default::default()
        };
        s.devices = vec![
            DeviceInfo {
                index: 0,
                name: "Mock RTX 4070 Ti".into(),
                uuid: "GPU-mock-0".into(),
            },
            DeviceInfo {
                index: 1,
                name: "Mock H100".into(),
                uuid: "GPU-mock-1".into(),
            },
        ];
        s
    }

    pub fn push_sample(&mut self, index: u32, util: f32, temp: f32, power_w: f32) {
        self.samples.push(Sample {
            vendor: "nvidia".to_string(),
            ts: SystemTime::now(),
            index,
            uuid: format!("GPU-mock-{index}"),
            name: format!("Mock GPU {index}"),
            util_gpu: Some(util),
            temp_gpu: Some(temp),
            power_w: Some(power_w),
            power_limit_w: Some(300.0),
            mem_used: 2 << 30,
            mem_total: 16 << 30,
            clocks_sm_mhz: Some(1800),
            clocks_max_sm_mhz: Some(2500),
            temp_slowdown: Some(90.0),
            temp_shutdown: Some(98.0),
            throttle_bits: if temp >= 90.0 { 0x020 } else { 0 },
            ..Default::default()
        });
    }

    pub fn push_proc(&mut self, pid: u32, gpu: u32, mem_gib: f32, cmd: &str) {
        self.procs.push(GpuProc {
            pid,
            gpu_index: gpu,
            kind: ProcKind::Compute,
            mem_used: (mem_gib * (1 << 30) as f32) as u64,
            sm_pct: Some(90.0),
            mem_pct: Some(70.0),
            enc_pct: None,
            dec_pct: None,
            cpu_pct: Some(300.0),
            uid: 1000,
            user: "tester".into(),
            command: cmd.into(),
            start_time: 1234,
        });
    }
}

impl Source for MockSource {
    fn environment(&mut self) -> Environment {
        self.env.clone().unwrap_or_default()
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, SourceError> {
        Ok(self.devices.clone())
    }

    fn sample(&mut self) -> Vec<Sample> {
        self.ticks += 1;
        self.samples.clone()
    }

    fn processes(&mut self) -> Vec<GpuProc> {
        self.procs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_source_contract() {
        let mut src = MockSource::two_gpu_loaded();
        src.push_sample(0, 90.0, 85.0, 250.0);
        src.push_proc(184722, 0, 13.4, "python train.py");
        let env = src.environment();
        assert_eq!(env.provider, "mock");
        assert_eq!(src.devices().unwrap().len(), 2);
        assert_eq!(src.sample().len(), 1);
        assert_eq!(src.ticks, 1);
        assert_eq!(src.processes()[0].command, "python train.py");
    }
}
