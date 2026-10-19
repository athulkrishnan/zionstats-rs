# Architecture

## Crates

```
zionstats (unified CLI)
 ├── zionstats-daemon  (zionstatsd bin + lib: tokio tasks, hyper server)
 │    └── zionstats-core (types, SHM, controller, sources, metrics)
 └── zionstats-tui     (ratatui SHM reader; NOT linked to nvml-wrapper)
      └── zionstats-core
```

`zionstats-core` depends on neither tokio, hyper, nor ratatui. Everything
in it is pure or GPU-free-testable: the NVML source lives behind the
`nvml` feature, next to `DemoSource` (scripted 120 s arcs) and
`MockSource` (deterministic tests).

## Daemon task model (tokio multi-thread)

- **poll task**: `Source::sample()` every `interval` (floor 0.25 s),
  fills `GpuSampleV1` rows, pushes history rings.
- **process task**: every 2nd tick, `Source::processes()` + /proc
  enrichment + `CgroupResolver` (LRU 1024, keyed on
  `(pid, starttime)` so PID reuse invalidates cleanly).
- **control task**: hottest GPU feeds the hysteresis controller; commands
  dispatch to plugs (HTTP, readback-verify x3) or the NVML fan actuator
  (opt-in). A `flock` on the SHM file guarantees a single controller.
- **publish**: one seqlock write per tick, shared into the hyper server's
  `LiveFrame` mutex for /metrics and /snapshot.
- **watchdog**: pings sd_notify every WATCHDOG_USEC/4 when running under
  systemd; a stalled loop stops pinging and systemd restarts us (startup
  then re-arms driver-default fans).
- **signals**: SIGTERM/SIGINT restore driver fan control, apply the plug
  shutdown policy, notify STOPPING, exit 0. SIGHUP reload is v0.6.

## Invariants

1. **The TUI never initializes NVML.** It is not linked to nvml-wrapper
   (feature graph enforces this) and only reads the SHM file.
2. **Readers never block the writer** and vice versa (seqlock; bounded
   retry with a stale fallback instead of an error).
3. **The daemon is the sole SHM writer** (flock; a second start refuses
   with "another zionstats writer holds").
4. **Core is GPU-free testable**: `cargo test --workspace` passes without
   a GPU; NVML code paths are exercised via Demo/Mock sources.
5. **Fail-safe direction on every error path**: fan control falls back to
   driver default, plugs follow the shutdown policy, stale samples force
   actuation off.

## Failure posture

- actuator write fails -> retry x3 -> driver default + alert event
- sample stale (> 15 s) -> force actuation off, in every actuating mode
- panic in a task -> logged, watchdog/restart path re-arms safe state
- daemon killed -9 -> startup always calls `set_default_fan_speed` before
  the first control tick, so a dead session cannot leave manual PWM behind

## v0.7 additions to the task model

- The poll loop now refreshes weather on the provider's own cadence
  (Open-Meteo, shared disk cache with the Python stack) and feeds live
  ambient + weather age into the control tick; stale weather forces an
  engaged plug OFF in auto+temp (usage trigger never reads weather).
- SIGHUP sets a reload flag consumed between ticks: the control stack
  is rebuilt (fan restore via Drop, plugs untouched, actuation belief
  resets off, readback reconcile resyncs within one period).
- A periodic readback window (readback_period_s) queries plug status
  and wall-power meters; definitive all-plug statuses feed the
  controller's external resync path.
