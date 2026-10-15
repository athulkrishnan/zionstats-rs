//! Thermal hysteresis controller — a 1:1 port of the Python
//! `zionstats/cooling.py` state machine (same defaults, same evaluation
//! order, same quirks — e.g. the duty cap clears `on` even in dry_run and
//! even when actuation is skipped). All clock time is injectable so the
//! machine is table-testable, mirroring `tests/test_cooling.py`.

pub mod actuators;
pub mod curve;
pub mod plugs_http;

use std::collections::VecDeque;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoolingMode {
    #[default]
    Off,
    DryRun,
    Auto,
    Manual,
}

impl CoolingMode {
    pub fn parse(s: &str) -> Option<CoolingMode> {
        match s {
            "off" => Some(CoolingMode::Off),
            "dry_run" => Some(CoolingMode::DryRun),
            "auto" => Some(CoolingMode::Auto),
            "manual" => Some(CoolingMode::Manual),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CoolingMode::Off => "off",
            CoolingMode::DryRun => "dry_run",
            CoolingMode::Auto => "auto",
            CoolingMode::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Trigger {
    #[default]
    Temp,
    Usage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    PlugOn,
    PlugOff,
    /// Manual PWM percent for a specific GPU index.
    Pwm(u32, u32),
    /// Give the fan back to the driver (failsafe / shutdown / disengage).
    DriverDefault(u32),
    Hold,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControlConfig {
    pub mode: CoolingMode,
    pub trigger: Trigger,
    pub trigger_temp: f32,
    pub resume_temp: f32,
    pub min_delta_c: f32,
    pub min_on_s: f32,
    pub min_off_s: f32,
    pub max_duty_min: f32,
    pub usage_util: f32,
    pub idle_off_s: f32,
    pub max_sample_age_s: f32,
    pub verify_delay_s: f32,
    pub ambient_temp_fallback_c: f32,
}

impl Default for ControlConfig {
    fn default() -> Self {
        // Python CoolingConfig defaults — pinned by tests/test_cooling.py.
        ControlConfig {
            mode: CoolingMode::Off,
            trigger: Trigger::Temp,
            trigger_temp: 82.0,
            resume_temp: 78.0,
            min_delta_c: 8.0,
            min_on_s: 120.0,
            min_off_s: 180.0,
            max_duty_min: 90.0,
            usage_util: 5.0,
            idle_off_s: 600.0,
            max_sample_age_s: 15.0,
            verify_delay_s: 0.4,
            ambient_temp_fallback_c: 30.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ControlState {
    pub mode: CoolingMode,
    pub on: bool,
    pub would_on: bool,
    pub reason: String,
    pub last_change_ts: f64,
    pub on_since: Option<f64>,
    pub duty_seconds: f64,
    pub plug_ok: Option<bool>,
    pub inhibited: String,
    pub idle_since: Option<f64>,
    pub util_pct: Option<f32>,
    pub pwm_engaged: bool,
    pub pwm_current: Option<u32>,
    /// set on external-readback mismatch; bypasses min-on/off for the
    /// immediate re-fire
    pub external_resync: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    On,
    Off,
    WouldOn,
    WouldOff,
    Fail,
    Inhibit,
    Duty,
    PwmSet,
    PwmDefault,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::On => "on",
            EventKind::Off => "off",
            EventKind::WouldOn => "would-on",
            EventKind::WouldOff => "would-off",
            EventKind::Fail => "fail",
            EventKind::Inhibit => "inhibit",
            EventKind::Duty => "duty",
            EventKind::PwmSet => "pwm-set",
            EventKind::PwmDefault => "pwm-default",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ControlEvent {
    pub ts: f64,
    pub kind: EventKind,
    pub msg: String,
}

/// Per-tick inputs. `None` mirrors Python's missing sample/metric.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickInputs {
    pub temp_c: Option<f32>,
    pub util_pct: Option<f32>,
    /// age of the sample in seconds (None = no sample at all)
    pub sample_age_s: Option<f64>,
    /// ambient override (config ambient in v0.5; weather is future work)
    pub ambient_c: Option<f32>,
    /// periodic external plug readback (None = not this tick).  When the
    /// readback disagrees with the controller's belief, the belief is
    /// resynced and `external_resync` bypasses min-on/off on the
    /// immediate re-fire (the relay was changed externally, so our
    /// anti-chatter windows do not apply to it).
    pub readback: Option<bool>,
}

pub type Clock = Box<dyn Fn() -> f64 + Send>;

pub fn system_clock() -> Clock {
    Box::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    })
}

pub struct Controller {
    pub cfg: ControlConfig,
    pub state: ControlState,
    events: VecDeque<ControlEvent>,
    clock: Clock,
    desired: Option<bool>,
    inhibited_alerted: bool,
    fail_alerted: bool,
    last_would: Option<bool>,
    /// Set when the duty cap fires: the resulting OFF must clear `on`
    /// regardless of actuation success (Python behavior — the safe
    /// direction). The caller reads and clears this after dispatch.
    pub pending_unconditional_off: bool,
}

impl Controller {
    pub fn new(cfg: ControlConfig, clock: Clock) -> Self {
        let mode = cfg.mode;
        Controller {
            cfg,
            state: ControlState {
                mode,
                external_resync: false,
                ..Default::default()
            },
            events: VecDeque::with_capacity(100),
            clock,
            desired: None,
            inhibited_alerted: false,
            fail_alerted: false,
            last_would: None,
            pending_unconditional_off: false,
        }
    }

    fn emit(&mut self, kind: EventKind, msg: String) {
        let ev = ControlEvent {
            ts: (self.clock)(),
            kind,
            msg,
        };
        self.events.push_back(ev);
        if self.events.len() > 100 {
            self.events.pop_front();
        }
    }

    pub fn take_events(&mut self) -> Vec<ControlEvent> {
        self.events.drain(..).collect()
    }

    fn mmss(sec: f64) -> String {
        let total = sec.max(0.0) as u64;
        format!("{}:{:02}", total / 60, total % 60)
    }

    fn ambient(&self, in_ambient: Option<f32>) -> f32 {
        in_ambient.unwrap_or(self.cfg.ambient_temp_fallback_c)
    }

    fn decide(&self, on: bool, tgpu: f32, tamb: f32) -> (bool, String) {
        if on {
            if tgpu <= self.cfg.resume_temp {
                return (
                    false,
                    format!("gpu {:.0}C <= resume {:.0}C", tgpu, self.cfg.resume_temp),
                );
            }
            return (
                true,
                format!("holding (resume at {:.0}C)", self.cfg.resume_temp),
            );
        }
        if tgpu >= self.cfg.trigger_temp {
            if (tgpu - tamb) >= self.cfg.min_delta_c {
                return (
                    true,
                    format!(
                        "gpu {:.0}C >= trigger {:.0}C, delta {:.0}C",
                        tgpu,
                        self.cfg.trigger_temp,
                        tgpu - tamb
                    ),
                );
            }
            return (
                false,
                format!(
                    "gpu hot but delta {:.0}C < {:.0}C (blower won't help)",
                    tgpu - tamb,
                    self.cfg.min_delta_c
                ),
            );
        }
        (
            false,
            format!("gpu {:.0}C < trigger {:.0}C", tgpu, self.cfg.trigger_temp),
        )
    }

    /// Usage-trigger decision. Mutates idle_since/util exactly like the
    /// Python `_decide_usage` (side effects inside the decision).
    fn decide_usage(&mut self, util: Option<f32>, tgpu: Option<f32>, now: f64) -> (bool, String) {
        let th = self.cfg.usage_util;
        let Some(util) = util else {
            self.state.idle_since = None;
            return (self.state.on, "GPU util unknown -- holding".to_string());
        };
        self.state.util_pct = Some(util);
        if util >= th {
            self.state.idle_since = None;
            return (true, format!("usage {:.0}% >= {:.0}%", util, th));
        }
        let idle_since = *self.state.idle_since.get_or_insert(now);
        let idle = now - idle_since;
        if !self.state.on {
            return (
                false,
                format!("idle {} (on at usage >= {:.0}%)", Self::mmss(idle), th),
            );
        }
        if let Some(t) = tgpu {
            if t >= self.cfg.resume_temp {
                return (
                    true,
                    format!(
                        "idle {}, hot {:.0}C >= guard {:.0}C",
                        Self::mmss(idle),
                        t,
                        self.cfg.resume_temp
                    ),
                );
            }
        }
        let remaining = self.cfg.idle_off_s as f64 - idle;
        if remaining > 0.0 {
            return (
                true,
                format!(
                    "idle {}, off in {}",
                    Self::mmss(idle),
                    Self::mmss(remaining)
                ),
            );
        }
        (
            false,
            format!(
                "idle {} >= {:.0}min, cool",
                Self::mmss(idle),
                self.cfg.idle_off_s / 60.0
            ),
        )
    }

    /// One control tick for the plug path. Returns the command to apply;
    /// the caller runs `verify_and_retry` and calls `commit` with the
    /// result (keeping the controller free of I/O).
    pub fn tick(&mut self, hot: &TickInputs) -> Command {
        let now = (self.clock)();
        let st_mode = self.cfg.mode;
        self.state.mode = st_mode;
        self.pending_unconditional_off = false;

        if st_mode == CoolingMode::Off {
            self.state.would_on = false;
            self.state.reason = "cooling disabled in settings".to_string();
            self.state.inhibited.clear();
            if self.state.on {
                return Command::PlugOff;
            }
            return Command::Hold;
        }

        let Some(tgpu) = hot.temp_c else {
            self.state.reason = "no GPU sample".to_string();
            return Command::Hold;
        };

        let sample_age = hot.sample_age_s.unwrap_or(f64::INFINITY);
        let stale_sample = sample_age > self.cfg.max_sample_age_s as f64;
        // v0.5: ambient is static config => weather is never stale. The
        // stale-weather inhibit branch is reserved for the future weather
        // source (Python: stale weather blocks auto+temp only).
        let stale_weather = false;

        let (mut want, why) = match self.cfg.trigger {
            Trigger::Usage => self.decide_usage(hot.util_pct, Some(tgpu), now),
            Trigger::Temp => self.decide(self.state.on, tgpu, self.ambient(hot.ambient_c)),
        };
        self.state.would_on = want;
        self.state.reason = why.clone();

        if self.cfg.mode == CoolingMode::Manual {
            want = self.desired.unwrap_or(self.state.on);
        }

        // external-readback reconciliation: the plug relay was toggled
        // outside this process (vendor app, voice assistant) -- resync
        // the belief.  Do NOT update last_change_ts here: that would
        // trigger the min-off hold and block the re-fire.  Instead, the
        // external_resync flag bypasses min-on/off for the immediate
        // re-fire, and last_change_ts is updated by commit().
        if let Some(rb) = hot.readback {
            if rb != self.state.on {
                self.state.on = rb;
                self.state.on_since = if rb { Some(now) } else { None };
                self.state.external_resync = true;
            }
        }

        // duty cap (always enforced when actually on — Python order quirk:
        // this runs before the dry_run return, and clears `on` even in
        // dry_run / even when mode != auto does not actuate)
        if self.state.on {
            if let Some(on_since) = self.state.on_since {
                self.state.duty_seconds = now - on_since;
                if self.state.duty_seconds > f64::from(self.cfg.max_duty_min * 60.0f32) {
                    self.emit(
                        EventKind::Duty,
                        format!(
                            "max continuous duty {:.0}min reached -- forced cooldown",
                            self.cfg.max_duty_min
                        ),
                    );
                    self.pending_unconditional_off = true;
                    return Command::PlugOff;
                }
            }
        }

        // failsafes: stale telemetry force-off applies in every actuating
        // mode (Python: `stale_sample or mode == "auto"`)
        let mut inhibit = "";
        if stale_sample {
            inhibit = "GPU samples stale";
        } else if stale_weather
            && self.cfg.trigger != Trigger::Usage
            && st_mode == CoolingMode::Auto
        {
            inhibit = "weather data stale";
        }
        if !inhibit.is_empty() && self.state.on && (stale_sample || st_mode == CoolingMode::Auto) {
            self.state.inhibited = inhibit.to_string();
            if !self.inhibited_alerted {
                self.emit(EventKind::Inhibit, format!("blower force-off: {inhibit}"));
                self.inhibited_alerted = true;
            }
            return Command::PlugOff;
        }
        if inhibit.is_empty() {
            self.state.inhibited.clear();
            self.inhibited_alerted = false;
        }

        if self.cfg.mode == CoolingMode::DryRun {
            if Some(want) != self.last_would {
                let kind = if want {
                    EventKind::WouldOn
                } else {
                    EventKind::WouldOff
                };
                self.emit(
                    kind,
                    format!(
                        "dry-run: would turn blower {} -- {}",
                        if want { "ON" } else { "OFF" },
                        why
                    ),
                );
                self.last_would = Some(want);
            }
            self.state.would_on = want;
            return Command::Hold;
        }

        if self.cfg.mode == CoolingMode::Manual {
            if want != self.state.on {
                return if want {
                    Command::PlugOn
                } else {
                    Command::PlugOff
                };
            }
            return Command::Hold;
        }

        // timing constraints (auto); external resync bypasses min-on/off
        if want != self.state.on {
            if self.state.external_resync {
                self.state.external_resync = false;
                return if want {
                    Command::PlugOn
                } else {
                    Command::PlugOff
                };
            }
            let since = self.state.last_change_ts;
            if self.state.on && now - since < self.cfg.min_on_s as f64 {
                self.state.reason = format!("{} [min-on hold]", why);
                return Command::Hold;
            }
            if !self.state.on && now - since < self.cfg.min_off_s as f64 {
                self.state.reason = format!("{} [min-off hold]", why);
                return Command::Hold;
            }
            return if want {
                Command::PlugOn
            } else {
                Command::PlugOff
            };
        }
        Command::Hold
    }

    /// Commit the result of applying a plug command (ported from `_apply`'s
    /// state bookkeeping). `ok` = every plug confirmed. Handles the
    /// duty-cap unconditional-off semantics via `pending_unconditional_off`.
    pub fn commit(&mut self, desired: bool, ok: bool, reason: &str) {
        let now = (self.clock)();
        if self.pending_unconditional_off {
            self.pending_unconditional_off = false;
            // Python: `_apply` runs, then `st.on = False` unconditionally.
            self.state.on = false;
            self.state.on_since = None;
            self.state.reason = reason.to_string();
            if ok {
                self.state.plug_ok = Some(true);
            } else {
                self.state.plug_ok = Some(false);
            }
            return;
        }
        if ok {
            self.state.on = desired;
            self.state.last_change_ts = now;
            self.state.on_since = if desired { Some(now) } else { None };
            self.state.reason = reason.to_string();
            self.state.plug_ok = Some(true);
            self.emit(
                if desired {
                    EventKind::On
                } else {
                    EventKind::Off
                },
                reason.to_string(),
            );
            self.fail_alerted = false;
        } else {
            self.state.plug_ok = Some(false);
            if !self.fail_alerted {
                self.emit(
                    EventKind::Fail,
                    format!(
                        "blower plug did not confirm {} after retries",
                        if desired { "ON" } else { "OFF" }
                    ),
                );
                self.fail_alerted = true;
            }
        }
        if desired && self.state.on {
            self.state.on_since = self.state.on_since.or(Some(now));
        }
    }

    /// TUI-style manual toggle (Python `set_manual`): records the desire
    /// and, in manual mode, returns the command to apply immediately.
    pub fn set_manual(&mut self, desired: bool) -> Command {
        self.desired = Some(desired);
        if self.cfg.mode == CoolingMode::Manual && desired != self.state.on {
            return if desired {
                Command::PlugOn
            } else {
                Command::PlugOff
            };
        }
        Command::Hold
    }

    pub fn manual_desired(&self) -> Option<bool> {
        self.desired
    }

    pub fn set_mode(&mut self, mode: CoolingMode) -> Command {
        self.cfg.mode = mode;
        self.state.mode = mode;
        self.state.idle_since = None; // mode excursion must not leave a stale idle clock
        if mode == CoolingMode::Off {
            return Command::PlugOff; // caller applies + commits
        }
        if mode == CoolingMode::Manual {
            self.desired = Some(self.state.on);
        }
        Command::Hold
    }

    pub fn latest_events(&self) -> &VecDeque<ControlEvent> {
        &self.events
    }
}

/// Test/daemon helper clock: shared mutable f64 seconds.
pub type SharedClock = std::sync::Arc<Mutex<f64>>;

pub fn shared_clock(start: f64) -> (SharedClock, Clock) {
    let inner: SharedClock = std::sync::Arc::new(Mutex::new(start));
    let inner2 = inner.clone();
    let f: Clock = Box::new(move || *inner2.lock().unwrap());
    (inner, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        clock: SharedClock,
        ctl: Controller,
    }

    fn env(mode: CoolingMode) -> Env {
        let (clock, f) = shared_clock(1000.0);
        let ctl = Controller::new(
            ControlConfig {
                mode,
                ..Default::default()
            },
            f,
        );
        Env { clock, ctl }
    }

    fn advance(env: &Env, s: f64) {
        *env.clock.lock().unwrap() += s;
    }

    fn hot_temp(t: f32) -> TickInputs {
        TickInputs {
            temp_c: Some(t),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
            ..Default::default()
        }
    }

    #[test]
    fn off_mode_holds_and_reports() {
        let mut e = env(CoolingMode::Off);
        assert_eq!(e.ctl.tick(&hot_temp(90.0)), Command::Hold);
        assert_eq!(e.ctl.state.reason, "cooling disabled in settings");
    }

    #[test]
    fn temp_trigger_on_and_off_hysteresis() {
        let mut e = env(CoolingMode::Auto);
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::PlugOn);
        e.ctl.commit(true, true, "test");
        assert!(e.ctl.state.on);
        assert_eq!(e.ctl.tick(&hot_temp(80.0)), Command::Hold); // holding band
        assert_eq!(e.ctl.tick(&hot_temp(70.0)), Command::Hold); // min-on hold
        assert!(e.ctl.state.reason.contains("min-on hold"));
        advance(&e, 121.0);
        assert_eq!(e.ctl.tick(&hot_temp(70.0)), Command::PlugOff);
        e.ctl.commit(false, true, "test");
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::Hold); // min-off hold
        assert!(e.ctl.state.reason.contains("min-off hold"));
        advance(&e, 181.0);
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::PlugOn);
    }

    #[test]
    fn min_delta_blocks_trigger() {
        let mut e = env(CoolingMode::Auto);
        // hot but only 4C over ambient: blower would not help
        let muggy = TickInputs {
            temp_c: Some(84.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(80.0),
            readback: None,
            ..Default::default()
        };
        assert_eq!(e.ctl.tick(&muggy), Command::Hold);
        assert!(e.ctl.state.reason.contains("blower won't help"));
    }

    #[test]
    fn stale_sample_forces_off_in_auto_and_manual() {
        for mode in [CoolingMode::Auto, CoolingMode::Manual] {
            let mut e = env(mode);
            if mode == CoolingMode::Manual {
                assert_eq!(e.ctl.set_manual(true), Command::PlugOn);
            }
            assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::PlugOn);
            e.ctl.commit(true, true, "test");
            let stale = TickInputs {
                temp_c: Some(85.0),
                sample_age_s: Some(16.0),
                ambient_c: Some(30.0),
                readback: None,
                ..Default::default()
            };
            assert_eq!(e.ctl.tick(&stale), Command::PlugOff);
            e.ctl.commit(false, true, "failsafe: GPU samples stale");
            assert_eq!(e.ctl.state.inhibited, "GPU samples stale");
            let inhibit_events: Vec<_> = e
                .ctl
                .latest_events()
                .iter()
                .filter(|ev| ev.kind == EventKind::Inhibit)
                .collect();
            assert_eq!(inhibit_events.len(), 1, "inhibit alerts once");
        }
    }

    #[test]
    fn duty_cap_forces_cooldown_unconditionally() {
        let mut e = env(CoolingMode::DryRun);
        // reach ON state via manual excursion
        e.ctl.set_mode(CoolingMode::Manual);
        assert_eq!(e.ctl.set_manual(true), Command::PlugOn);
        e.ctl.commit(true, true, "test");
        e.ctl.set_mode(CoolingMode::DryRun);
        advance(&e, 91.0 * 60.0);
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::PlugOff);
        e.ctl.commit(false, false, "duty cap reached");
        assert!(
            !e.ctl.state.on,
            "duty cap clears state even if apply failed"
        );
        let duty_events: Vec<_> = e
            .ctl
            .latest_events()
            .iter()
            .filter(|ev| ev.kind == EventKind::Duty)
            .collect();
        assert_eq!(duty_events.len(), 1);
    }

    #[test]
    fn dry_run_never_actuates_but_logs_once() {
        let mut e = env(CoolingMode::DryRun);
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::Hold);
        assert!(e.ctl.state.would_on);
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::Hold); // no repeat
        let would: Vec<_> = e
            .ctl
            .latest_events()
            .iter()
            .filter(|ev| ev.kind == EventKind::WouldOn)
            .collect();
        assert_eq!(would.len(), 1);
        assert!(!e.ctl.state.on);
    }

    #[test]
    fn usage_trigger_lifecycle() {
        let mut e = env(CoolingMode::Auto);
        e.ctl.cfg.trigger = Trigger::Usage;
        let load = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(e.ctl.tick(&load), Command::PlugOn);
        e.ctl.commit(true, true, "test");
        let idle_hot = TickInputs {
            temp_c: Some(80.0),
            util_pct: Some(0.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(e.ctl.tick(&idle_hot), Command::Hold);
        assert!(e.ctl.state.reason.contains("hot"));
        let idle_cool = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(0.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(e.ctl.tick(&idle_cool), Command::Hold, "within idle_off_s");
        advance(&e, 601.0);
        assert_eq!(e.ctl.tick(&idle_cool), Command::PlugOff);
    }

    #[test]
    fn usage_unknown_util_holds_and_breaks_idle() {
        let mut e = env(CoolingMode::Auto);
        e.ctl.cfg.trigger = Trigger::Usage;
        let load = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(e.ctl.tick(&load), Command::PlugOn);
        e.ctl.commit(true, true, "test");
        let unknown = TickInputs {
            temp_c: Some(50.0),
            util_pct: None,
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(e.ctl.tick(&unknown), Command::Hold);
        assert!(e.ctl.state.idle_since.is_none());
    }

    #[test]
    fn no_sample_holds_without_crash() {
        let mut e = env(CoolingMode::Auto);
        assert_eq!(e.ctl.tick(&TickInputs::default()), Command::Hold);
        assert_eq!(e.ctl.state.reason, "no GPU sample");
    }

    #[test]
    fn min_on_hold_blocks_retrigger_within_window() {
        let mut e = env(CoolingMode::Auto);
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::PlugOn);
        e.ctl.commit(true, true, "test");
        advance(&e, 10.0);
        // dropped below resume quickly: OFF wanted but min-on holds
        assert_eq!(e.ctl.tick(&hot_temp(60.0)), Command::Hold);
        assert!(e.ctl.state.reason.contains("[min-on hold]"));
    }

    #[test]
    fn fail_commit_alerts_once_then_recovers() {
        let mut e = env(CoolingMode::Auto);
        assert_eq!(e.ctl.tick(&hot_temp(85.0)), Command::PlugOn);
        e.ctl.commit(true, false, "test"); // actuation failed
        assert_eq!(e.ctl.state.plug_ok, Some(false));
        e.ctl.tick(&hot_temp(85.0));
        e.ctl.commit(true, false, "test"); // still failing: no new fail event
        let fails: Vec<_> = e
            .ctl
            .latest_events()
            .iter()
            .filter(|ev| ev.kind == EventKind::Fail)
            .collect();
        assert_eq!(fails.len(), 1, "fail alerts once until recovery");
        e.ctl.commit(true, true, "test"); // recovered
        assert_eq!(e.ctl.state.plug_ok, Some(true));
    }
}

#[cfg(test)]
mod desync_tests {
    use super::*;

    #[test]
    fn external_off_resync_and_refire() {
        let (clock, f) = shared_clock(1000.0);
        let mut ctl = Controller::new(
            ControlConfig {
                mode: CoolingMode::Auto,
                trigger: Trigger::Usage,
                ..Default::default()
            },
            f,
        );
        let load = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(ctl.tick(&load), Command::PlugOn);
        ctl.commit(true, true, "test");
        assert!(ctl.state.on);

        // user flips the plug off in the Tuya app; readback sees it
        let ext_off = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: Some(false),
        };
        let cmd = ctl.tick(&ext_off);
        assert!(!ctl.state.on, "belief must resync to physical OFF");
        assert_eq!(cmd, Command::PlugOn, "re-fire bypasses min-off on resync");

        // next tick: want=True again -> re-engages (clock has advanced
        // past min_off in real usage)
        *clock.lock().unwrap() += 200.0;
        assert_eq!(ctl.tick(&load), Command::PlugOn);
    }

    #[test]
    fn external_on_resync_when_believing_off() {
        let (clock, f) = shared_clock(1000.0);
        let mut ctl = Controller::new(
            ControlConfig {
                mode: CoolingMode::Auto,
                trigger: Trigger::Usage,
                ..Default::default()
            },
            f,
        );
        let ext_on = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: Some(true),
        };
        ctl.tick(&ext_on);
        assert!(ctl.state.on, "belief must resync to physical ON");
    }

    #[test]
    fn readback_none_is_noop() {
        let (clock, f) = shared_clock(1000.0);
        let mut ctl = Controller::new(
            ControlConfig {
                mode: CoolingMode::Auto,
                trigger: Trigger::Usage,
                ..Default::default()
            },
            f,
        );
        let no_rb = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(ctl.tick(&no_rb), Command::PlugOn);
        ctl.commit(true, true, "test");
        let base = ctl.state.on;
        ctl.tick(&no_rb);
        assert_eq!(ctl.state.on, base, "None readback must not touch belief");
    }

    #[test]
    fn external_resync_bypasses_min_off_hold() {
        let (clock, f) = shared_clock(1000.0);
        let mut ctl = Controller::new(
            ControlConfig {
                mode: CoolingMode::Auto,
                ..Default::default()
            },
            f,
        );
        let load = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(40.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(ctl.tick(&load), Command::PlugOn);
        ctl.commit(true, true, "test");

        // without resync: load drops -> OFF wanted but min-on holds
        *clock.lock().unwrap() += 10.0;
        let cool = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(0.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: None,
        };
        assert_eq!(ctl.tick(&cool), Command::Hold, "min-on hold");

        // with resync: bypass fires (relay was changed externally)
        let cool_rb = TickInputs {
            temp_c: Some(50.0),
            util_pct: Some(0.0),
            sample_age_s: Some(0.0),
            ambient_c: Some(30.0),
            readback: Some(false),
        };
        assert_eq!(
            ctl.tick(&cool_rb),
            Command::PlugOff,
            "external resync bypasses min-off"
        );
    }
}
