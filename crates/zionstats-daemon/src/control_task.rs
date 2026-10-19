//! Daemon-side control wiring: builds the controller + actuators from
//! config, runs the per-tick decision/dispatch, and mirrors state into SHM.
//! Safety posture (ported): fan PWM is opt-in (`[daemon] fan_control`),
//! `--readonly` structurally builds no actuators, and startup always
//! restores driver-default fans before the first control tick.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "nvml")]
use zionstats_core::control::actuators::FanActuator;
#[cfg_attr(test, allow(unused_imports))]
use zionstats_core::control::actuators::{verify_and_retry, Plug};
use zionstats_core::control::plugs;
#[cfg(test)]
use zionstats_core::control::ControlConfig;
use zionstats_core::control::{Command, ControlState, Controller, CoolingMode, TickInputs};
use zionstats_core::shm::layout::ControlStateV1;

use crate::daemon::DaemonOpts;

#[derive(Default)]
pub struct ControlStack {
    pub controller: Option<Controller>,
    pub plugs: Vec<Box<dyn Plug>>,
    pub fans: Option<Box<dyn FanActuator>>,
    /// event log mirrored into the SHM alert ring
    pub events: Vec<(u64, u8, String)>,
    pub next_event_seq: u64,
}

impl ControlStack {
    /// Build from config. Returns (stack, warnings). `--readonly` builds
    /// nothing actuating at all.
    pub fn build(opts: &DaemonOpts, cfg: &zionstats_core::config::Config) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut stack = ControlStack::default();
        if opts.readonly || cfg.readonly || cfg.daemon.readonly {
            warnings.push("readonly: actuators disabled (--readonly or config)".into());
            return (stack, warnings);
        }
        for p in &cfg.plugs {
            match plugs::from_config(p) {
                Ok(Some(plug)) => stack.plugs.push(plug),
                Ok(None) => warnings.push(format!(
                    "plug {:?}: backend {:?} skipped (missing optional fields)",
                    p.name, p.backend
                )),
                Err(e) => warnings.push(format!("plug {:?}: {}", p.name, e)),
            }
        }
        if !stack.plugs.is_empty() {
            let mut cc = cfg.cooling.clone();
            if let Some(m) = opts.mode_override {
                cc.mode = m;
            }
            stack.controller = Some(Controller::new(cc, zionstats_core::control::system_clock()));
        }
        #[cfg(feature = "nvml")]
        if cfg.daemon.fan_control {
            match crate::nvml_fan::NvmlFan::try_new() {
                Ok(f) => {
                    let caps = f.capabilities();
                    if caps.writable {
                        // startup restore: never inherit a crashed session's PWM
                        if let Err(e) = f.restore_all_default() {
                            warnings.push(format!("fan startup restore: {e}"));
                        }
                        stack.fans = Some(Box::new(f));
                    } else {
                        warnings.push(
                            "fan_control: NVML fans not writable (root? coolbits?) -- driver keeps control"
                                .into(),
                        );
                    }
                }
                Err(e) => warnings.push(format!("fan_control: {e}")),
            }
        }
        (stack, warnings)
    }

    /// One control tick. Blocking (HTTP verify loop); call from
    /// spawn_blocking. Returns the updated state for SHM.
    pub fn tick(&mut self, hot: &TickInputs, force_shutdown: &AtomicBool) -> Option<ControlState> {
        let ctl = self.controller.as_mut()?;
        let cmd = ctl.tick(hot);
        match cmd {
            Command::PlugOn | Command::PlugOff => {
                let desired = cmd == Command::PlugOn;
                let reason = ctl.state.reason.clone();
                let delay = ctl.cfg.verify_delay_s;
                let mut ok_all = true;
                for plug in self.plugs.iter_mut() {
                    let mut noop_sleep = |_s: f64| {
                        // verify loop needs a real pause between write and
                        // readback; ~0.4s from config
                        std::thread::sleep(std::time::Duration::from_secs_f64(delay as f64));
                    };
                    let ok = verify_and_retry(plug.as_mut(), desired, delay, 3, &mut noop_sleep);
                    if !ok {
                        ok_all = false;
                    }
                }
                ctl.commit(desired, ok_all, &reason);
            }
            Command::Pwm(gpu, pct) => {
                if let Some(fans) = self.fans.as_mut() {
                    if let Err(e) = fans.set_pwm(gpu, 0, pct) {
                        let _ = fans.restore_default(gpu);
                        let _ = e;
                    } else {
                        ctl.state.pwm_engaged = true;
                        ctl.state.pwm_current = Some(pct);
                    }
                }
            }
            Command::DriverDefault(gpu) => {
                if let Some(fans) = self.fans.as_mut() {
                    let _ = fans.restore_default(gpu);
                    ctl.state.pwm_engaged = false;
                    ctl.state.pwm_current = None;
                }
            }
            Command::Hold => {}
        }
        // graceful-shutdown plug policy (auto_off default)
        if force_shutdown.load(Ordering::SeqCst) && ctl.state.on {
            let mut ok_all = true;
            for plug in self.plugs.iter_mut() {
                let delay = ctl.cfg.verify_delay_s;
                let mut sleeper = |_s: f64| {
                    std::thread::sleep(std::time::Duration::from_secs_f64(delay as f64));
                };
                if !verify_and_retry(plug.as_mut(), false, delay, 3, &mut sleeper) {
                    ok_all = false;
                }
            }
            ctl.commit(false, ok_all, "shutdown policy");
        }

        // drain controller events into the event log
        for ev in ctl.take_events() {
            let kind_u8 = match ev.kind {
                zionstats_core::control::EventKind::On => 0,
                zionstats_core::control::EventKind::Off => 1,
                zionstats_core::control::EventKind::WouldOn => 2,
                zionstats_core::control::EventKind::WouldOff => 3,
                zionstats_core::control::EventKind::Fail => 4,
                zionstats_core::control::EventKind::Inhibit => 5,
                zionstats_core::control::EventKind::Duty => 6,
                zionstats_core::control::EventKind::PwmSet => 7,
                zionstats_core::control::EventKind::PwmDefault => 8,
            };
            self.next_event_seq += 1;
            self.events.push((self.next_event_seq, kind_u8, ev.msg));
            if self.events.len() > 64 {
                self.events.remove(0);
            }
        }

        Some(self.shm_state())
    }

    fn shm_state(&self) -> ControlState {
        self.controller
            .as_ref()
            .map(|c| c.state.clone())
            .unwrap_or_default()
    }

    pub fn state_for_shm(&self) -> Option<ControlStateV1> {
        let ctl = self.controller.as_ref()?;
        let st = &ctl.state;
        Some(ControlStateV1 {
            last_change_ms: (st.last_change_ts * 1000.0) as u64,
            on_since_ms: st.on_since.map(|s| (s * 1000.0) as u64).unwrap_or(0),
            duty_seconds: st.duty_seconds,
            reason: {
                let mut r = [0u8; 96];
                let bytes = st.reason.as_bytes();
                let n = bytes.len().min(96);
                r[..n].copy_from_slice(&bytes[..n]);
                r
            },
            mode: match ctl.cfg.mode {
                CoolingMode::Off => 0,
                CoolingMode::DryRun => 1,
                CoolingMode::Auto => 2,
                CoolingMode::Manual => 3,
            },
            trigger: match ctl.cfg.trigger {
                zionstats_core::control::Trigger::Temp => 0,
                zionstats_core::control::Trigger::Usage => 1,
            },
            state_on: st.on as u8,
            would_on: st.would_on as u8,
            plug_ok: st.plug_ok.map(|b| if b { 1i8 } else { 0 }).unwrap_or(-1),
            inhibited: (!st.inhibited.is_empty()) as u8,
            pwm_engaged: st.pwm_engaged as u8,
            _pad0: 0,
            pwm_current: st.pwm_current.map(|p| p as f32).unwrap_or(f32::NAN),
            ambient_c: ctl.cfg.ambient_temp_fallback_c,
            events_dropped: 0,
            _pad1: [0; 4],
        })
    }

    /// Sample every plug's meter; None readings are kept so callers can
    /// distinguish "no meter" from "zero watts".
    pub fn sample_power(&mut self) -> Vec<(String, Option<f64>)> {
        self.plugs
            .iter_mut()
            .map(|p| (p.name().to_string(), p.power()))
            .collect()
    }
}

/// Rectangular session-Wh integration (same fallback math as the Python
/// app): None readings update nothing; dt clamped so a stalled loop cannot
/// inject phantom Wh.
pub fn integrate_plug_energy(
    energy: &mut std::collections::HashMap<String, f64>,
    last: &mut std::collections::HashMap<String, f64>,
    prev_ts: Option<f64>,
    now_ts: f64,
    readings: &[(String, Option<f64>)],
) {
    for (name, w) in readings {
        let Some(w) = w else { continue };
        last.insert(name.clone(), *w);
        if let Some(prev) = prev_ts {
            let dt = (now_ts - prev).clamp(0.0, 120.0);
            *energy.entry(name.clone()).or_insert(0.0) += w * dt / 3600.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plug_energy_integration() {
        let mut energy = std::collections::HashMap::new();
        let mut last = std::collections::HashMap::new();
        integrate_plug_energy(
            &mut energy,
            &mut last,
            None,
            100.0,
            &[("blower".to_string(), Some(120.0))],
        );
        assert!(
            energy.is_empty() && last["blower"] == 120.0,
            "baseline only"
        );
        integrate_plug_energy(
            &mut energy,
            &mut last,
            Some(100.0),
            160.0,
            &[("blower".to_string(), Some(120.0))],
        );
        assert!((energy["blower"] - 120.0 * 60.0 / 3600.0).abs() < 1e-9);
        integrate_plug_energy(
            &mut energy,
            &mut last,
            Some(160.0),
            220.0,
            &[("ghost".to_string(), None)],
        );
        assert!(!energy.contains_key("ghost"), "None integrates nothing");
        // dt clamp: a stalled loop cannot inject phantom Wh
        integrate_plug_energy(
            &mut energy,
            &mut last,
            Some(0.0),
            1000.0,
            &[("blower".to_string(), Some(100.0))],
        );
        assert!((energy["blower"] - (2.0 + 100.0 * 120.0 / 3600.0)).abs() < 1e-9);
    }
    use zionstats_core::control::actuators::MockPlug;
    use zionstats_core::control::shared_clock;

    #[test]
    fn readonly_builds_nothing() {
        let cfg = zionstats_core::config::Config::default();
        let opts = DaemonOpts {
            readonly: true,
            ..Default::default()
        };
        let (stack, warnings) = ControlStack::build(&opts, &cfg);
        assert!(stack.controller.is_none() && stack.plugs.is_empty() && stack.fans.is_none());
        assert!(!warnings.is_empty());
    }

    #[test]
    fn plug_dispatch_end_to_end_with_mock() {
        // build a stack with a MockPlug wired in by hand (config path only
        // makes HttpPlugs); drive a usage-trigger ON/OFF cycle
        let (clock, f) = shared_clock(1000.0);
        let mut stack = ControlStack::default();
        let mut mock = MockPlug::new("m");
        mock.plug_name = "mock".into();
        stack.plugs.push(Box::new(mock));
        stack.controller = Some(Controller::new(
            ControlConfig {
                mode: CoolingMode::Auto,
                trigger: zionstats_core::control::Trigger::Usage,
                verify_delay_s: 0.0,
                ..Default::default()
            },
            f,
        ));
        let shutdown = AtomicBool::new(false);
        let load = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
            weather_age_s: Some(0.0),
        };
        let st = stack.tick(&load, &shutdown).unwrap();
        assert!(st.on, "usage load must turn the plug ON");
        // the mock confirmed via real verify loop (delay 0.0)
        assert_eq!(stack.plugs[0].status(), Some(true));

        // idle: first tick starts the idle timer, then jump past idle_off_s
        let idle = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(0.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
            weather_age_s: Some(0.0),
        };
        let st = stack.tick(&idle, &shutdown).unwrap();
        assert!(st.on, "idle window still running");
        *clock.lock().unwrap() += 700.0;
        let st = stack.tick(&idle, &shutdown).unwrap();
        assert!(!st.on, "idle_off_s elapsed + cooled -> OFF");
        assert_eq!(stack.plugs[0].status(), Some(false));
    }

    #[test]
    fn shutdown_policy_turns_plug_off() {
        let (_, f) = shared_clock(1000.0);
        let mut stack = ControlStack::default();
        stack.plugs.push(Box::new(MockPlug::new("m")));
        stack.controller = Some(Controller::new(
            ControlConfig {
                mode: CoolingMode::Auto,
                trigger: zionstats_core::control::Trigger::Usage,
                verify_delay_s: 0.0,
                ..Default::default()
            },
            f,
        ));
        let load = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
            weather_age_s: Some(0.0),
        };
        stack.tick(&load, &AtomicBool::new(false)).unwrap();
        let shutdown = AtomicBool::new(true);
        stack.tick(&load, &shutdown).unwrap();
        assert!(!stack.controller.as_ref().unwrap().state.on);
        assert_eq!(stack.plugs[0].status(), Some(false));
    }
}
