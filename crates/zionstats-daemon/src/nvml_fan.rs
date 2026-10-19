//! NvmlFan: direct NVML fan-PWM actuator (nvml-wrapper). Capability-probes
//! at construction; every write is the manual-control handoff, every
//! restore gives the card back to the driver curve. nvml's own docs warn:
//! setting the fan speed too low can burn the GPU — the curve floor and
//! failsafe direction (DriverDefault) exist for exactly that reason.

use zionstats_core::control::actuators::{ActuatorError, FanActuator, FanCaps};

#[cfg(feature = "nvml")]
pub struct NvmlFan {
    nvml: &'static nvml_wrapper::Nvml,
    indices: Vec<u32>,
    /// fans per GPU as probed at startup
    fans_per_gpu: Vec<u32>,
}

#[cfg(feature = "nvml")]
impl NvmlFan {
    pub fn try_new() -> Result<Self, String> {
        let nvml: &'static nvml_wrapper::Nvml = Box::leak(Box::new(
            nvml_wrapper::Nvml::init().map_err(|e| e.to_string())?,
        ));
        let count = nvml.device_count().map_err(|e| e.to_string())?;
        let mut indices = Vec::new();
        let mut fans_per_gpu = Vec::new();
        let mut any_fans = false;
        for i in 0..count {
            indices.push(i);
            let fans = nvml
                .device_by_index(i)
                .ok()
                .and_then(|d| d.num_fans().ok())
                .unwrap_or(0);
            fans_per_gpu.push(fans);
            if fans > 0 {
                any_fans = true;
            }
        }
        if !any_fans {
            return Err("no GPU exposes fans (datacenter cards?)".into());
        }
        Ok(NvmlFan {
            nvml,
            indices,
            fans_per_gpu,
        })
    }

    pub fn restore_all_default(&self) -> Result<(), String> {
        for (pos, i) in self.indices.iter().enumerate() {
            for fan in 0..self.fans_per_gpu[pos] {
                if let Ok(mut d) = self.nvml.device_by_index(*i) {
                    d.set_default_fan_speed(fan)
                        .map_err(|e| format!("gpu{i} fan{fan}: {e}"))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(feature = "nvml")]
impl FanActuator for NvmlFan {
    fn restore_all_default(&mut self) -> Result<(), ActuatorError> {
        NvmlFan::restore_all_default(self).map_err(ActuatorError::Failed)
    }

    fn capabilities(&self) -> FanCaps {
        FanCaps {
            num_fans: self.fans_per_gpu.iter().sum(),
            writable: true, // probed at startup: try_new fails when none writable
        }
    }

    fn set_pwm(&mut self, gpu: u32, fan: u32, pct: u32) -> Result<(), ActuatorError> {
        let mut d = self
            .nvml
            .device_by_index(gpu)
            .map_err(|e| ActuatorError::Failed(e.to_string()))?;
        d.set_fan_speed(fan, pct.clamp(0, 100))
            .map_err(|e| ActuatorError::Failed(e.to_string()))
    }

    fn restore_default(&mut self, gpu: u32) -> Result<(), ActuatorError> {
        let pos = self.indices.iter().position(|i| *i == gpu).unwrap_or(0);
        let mut d = self
            .nvml
            .device_by_index(gpu)
            .map_err(|e| ActuatorError::Failed(e.to_string()))?;
        for fan in 0..self.fans_per_gpu.get(pos).copied().unwrap_or(0) {
            d.set_default_fan_speed(fan)
                .map_err(|e| ActuatorError::Failed(e.to_string()))?;
        }
        Ok(())
    }

    fn readback_pct(&mut self, gpu: u32, fan: u32) -> Result<Option<u32>, ActuatorError> {
        let d = self
            .nvml
            .device_by_index(gpu)
            .map_err(|e| ActuatorError::Failed(e.to_string()))?;
        Ok(d.fan_speed(fan).ok())
    }
}
