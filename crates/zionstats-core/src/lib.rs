//! zionstats-core: shared types, SHM layout (seqlock), thermal hysteresis
//! controller, telemetry sources, and metrics rendering. All pure or
//! GPU-free-testable; the daemon and TUI are thin shells over this crate.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod cgroup;
pub mod config;
pub mod control;
pub mod insights;
pub mod metrics;
pub mod procfs;
pub mod shm;
pub mod source;
pub mod types;

pub use control::actuators::{MockPlug, Plug, PlugError};
pub use control::{
    Command, ControlConfig, ControlEvent, ControlState, Controller, CoolingMode, TickInputs,
    Trigger,
};
pub use source::{Source, SourceError};
pub use types::{Sample, Verdict, MAX_GPUS};
