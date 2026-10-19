//! Fan/plug actuator traits + the readback-verify retry loop ported from
//! Python `_apply`. Shelly/Tasmota land in P4 (daemon crate) behind `Plug`;
//! here we define the trait, the mock, and the loop so the controller stays
//! pure and table-testable.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanCaps {
    pub num_fans: u32,
    pub writable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ActuatorError {
    #[error("actuator unavailable: {0}")]
    Unavailable(String),
    #[error("actuator command failed: {0}")]
    Failed(String),
}

pub trait FanActuator: Send {
    fn capabilities(&self) -> FanCaps;
    fn set_pwm(&mut self, gpu: u32, fan: u32, pct: u32) -> Result<(), ActuatorError>;
    fn restore_default(&mut self, gpu: u32) -> Result<(), ActuatorError>;
    /// Read back the current PWM percent; Ok(None) = unreadable (counts as
    /// verify success in the ported loop, matching Python's `None` readback).
    fn readback_pct(&mut self, gpu: u32, fan: u32) -> Result<Option<u32>, ActuatorError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PlugError {
    #[error("plug command failed: {0}")]
    Failed(String),
}

pub trait Plug: Send {
    fn name(&self) -> &str;
    fn on(&mut self) -> Result<(), PlugError>;
    fn off(&mut self) -> Result<(), PlugError>;
    /// None = unreachable / no readback support (Python parity: treated as
    /// verify success — the write-then-verify loop trusts the command).
    fn status(&mut self) -> Option<bool>;
    /// Live wall watts from the plug's metering hardware; None = no meter
    /// on this model/firmware. Never guessed.
    fn power(&mut self) -> Option<f64> {
        None
    }
}

/// Scriptable mock, mirroring Python MockPlug: records commands, can fail
/// N times, can lie on readback.
#[derive(Default)]
pub struct MockPlug {
    pub plug_name: String,
    pub state: bool,
    pub commands: Vec<&'static str>,
    pub fail_next: u32,
    pub lie_status: Option<bool>,
    /// scriptable wall-watt reading returned by `power()`
    pub power_w: Option<f64>,
}

impl MockPlug {
    pub fn new(name: &str) -> Self {
        MockPlug {
            plug_name: name.to_string(),
            ..Default::default()
        }
    }

    fn cmd(&mut self, desired: bool) -> Result<(), PlugError> {
        self.commands.push(if desired { "on" } else { "off" });
        if self.fail_next > 0 {
            self.fail_next -= 1;
            return Err(PlugError::Failed("scripted failure".to_string()));
        }
        self.state = desired;
        Ok(())
    }
}

impl Plug for MockPlug {
    fn name(&self) -> &str {
        &self.plug_name
    }
    fn on(&mut self) -> Result<(), PlugError> {
        self.cmd(true)
    }
    fn off(&mut self) -> Result<(), PlugError> {
        self.cmd(false)
    }
    fn status(&mut self) -> Option<bool> {
        Some(self.lie_status.unwrap_or(self.state))
    }
    fn power(&mut self) -> Option<f64> {
        self.power_w
    }
}

pub type SleepFn<'a> = &'a mut dyn FnMut(f64);

/// Ported verify loop (Python `_apply` inner): up to 3 attempts of
/// { command; sleep(verify_delay); readback; None-or-match = ok }.
/// `sleep` is in seconds and injectable (tests never really sleep).
pub fn verify_and_retry(
    plug: &mut dyn Plug,
    desired: bool,
    delay_s: f32,
    attempts: u32,
    sleep: SleepFn,
) -> bool {
    for _ in 0..attempts.max(1) {
        let cmd_result = if desired { plug.on() } else { plug.off() };
        if cmd_result.is_err() {
            continue;
        }
        sleep(delay_s as f64);
        match plug.status() {
            None => return true,
            Some(st) if st == desired => return true,
            _ => continue,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn verify_success_first_try() {
        let mut plug = MockPlug::new("m");
        let slept = Cell::new(0.0);
        let mut sleep = |s: f64| slept.set(s);
        assert!(verify_and_retry(&mut plug, true, 0.4, 3, &mut sleep));
        assert_eq!(plug.commands, vec!["on"]);
        assert!((slept.get() - 0.4).abs() < 1e-6, "f32 0.4 widened to f64");
    }

    #[test]
    fn retries_after_failure_then_succeeds() {
        let mut plug = MockPlug::new("m");
        plug.fail_next = 2;
        let mut sleep = |_| {};
        assert!(verify_and_retry(&mut plug, true, 0.1, 3, &mut sleep));
        assert_eq!(plug.commands, vec!["on", "on", "on"]);
    }

    #[test]
    fn three_failures_return_false() {
        let mut plug = MockPlug::new("m");
        plug.fail_next = 9;
        let mut sleep = |_| {};
        assert!(!verify_and_retry(&mut plug, false, 0.0, 3, &mut sleep));
        assert_eq!(plug.commands, vec!["off", "off", "off"]);
    }

    #[test]
    fn lying_status_forces_retry_until_readback_matches() {
        let mut plug = MockPlug::new("m");
        plug.lie_status = Some(false); // claims OFF while we command ON
        let mut sleep = |_| {};
        assert!(!verify_and_retry(&mut plug, true, 0.0, 3, &mut sleep));
        // now readback is honest
        plug.lie_status = None;
        let mut plug2 = MockPlug::new("m");
        let mut sleep2 = |_| {};
        assert!(verify_and_retry(&mut plug2, true, 0.0, 3, &mut sleep2));
    }

    #[test]
    fn none_readback_counts_as_success() {
        struct NoReadback;
        impl Plug for NoReadback {
            fn name(&self) -> &str {
                "noreadback"
            }
            fn on(&mut self) -> Result<(), PlugError> {
                Ok(())
            }
            fn off(&mut self) -> Result<(), PlugError> {
                Ok(())
            }
            fn status(&mut self) -> Option<bool> {
                None
            }
        }
        let mut p = NoReadback;
        let mut sleep = |_| {};
        assert!(verify_and_retry(&mut p, true, 0.0, 3, &mut sleep));
    }
}
