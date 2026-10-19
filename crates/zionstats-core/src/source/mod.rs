//! Telemetry sources. The `Source` trait abstracts NVML so the daemon,
//! tests, and CI run GPU-free via `MockSource`/`DemoSource`.

pub mod demo;
pub mod mock;
#[cfg(feature = "nvml")]
pub mod nvml;
#[cfg(feature = "nvml")]
pub mod nvml_pmon;
pub mod replay;

use crate::types::{DeviceInfo, Environment, GpuProc, Sample};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("NVML init failed: {0}")]
    Nvml(String),
    #[error("no GPUs visible")]
    NoDevices,
}

pub trait Source: Send {
    fn environment(&mut self) -> Environment;
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, SourceError>;
    /// One sample per GPU; per-field absence is `None`, never a hard error
    /// (Python `_g()` parity: "nothing here may raise at tick time").
    fn sample(&mut self) -> Vec<Sample>;
    fn processes(&mut self) -> Vec<GpuProc>;
}
