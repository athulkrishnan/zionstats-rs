# Security policy

## Scope

zionstats-rs is a local monitoring daemon and terminal UI. Its network
surface is deliberately small:

- **Metrics listener** (default `127.0.0.1:9400`, off only in the sense
  that nothing external should reach it): `/metrics`, `/snapshot`,
  `/healthz`. No authentication by design. `/metrics` includes the PIDs,
  usernames, and command lines of GPU processes unless
  `[export] expose_processes = false`. On shared/multi-user hosts, keep
  the loopback bind and consider disabling process exposure.
  Optionally set `[export] token` (config only, never CLI) to require
  a Bearer token on /metrics and /snapshot.
- **LAN plug control** (optional, off by default): Shelly and Tasmota
  plugs are driven over plain HTTP on your LAN (no TLS, same as the
  sibling Python project). Nothing is sent to any cloud.
  Note: a LAN man-in-the-middle can spoof plug verification readbacks
  seen by the control loop. On any actuator error or failed readback
  the daemon fails safe to driver-default fan control; do not treat
  plug state as a safety mechanism.
- **Fan PWM** (optional, off by default, root): hands GPU fans to manual
  control. The daemon restores driver control at startup, on SIGTERM,
  and after a watchdog restart, and falls back to driver default on any
  actuator error. NVML's own documentation warns that setting fan speeds
  too low can burn the GPU -- treat `fan_curve` as safety-critical
  configuration.

## Data at rest

- Shared memory `/dev/shm/zionstats-<uid>`: mode 0600, uid-scoped,
  telemetry only (no secrets). A `shm_group` chown widens it to one
  group deliberately.
- Config `~/.config/zionstats/config.toml` (or `$ZION_CONFIG`): can hold
  plug credentials; keep it 0600 (the Rust side never writes it).
- The daemon refuses to start twice (flock on the SHM file), so two
  controllers can never race the same plug.

## Hardening notes

The systemd unit ships with ProtectSystem=strict, ReadWritePaths=/dev/shm,
NoNewPrivileges, MemoryDenyWriteExecute and friends. CapabilityBoundingSet
is deliberately unset in v0.6 (some NVML paths misbehave under an empty
set); tightening is tracked as a follow-up with a test matrix.

## Reporting a vulnerability

GitHub private vulnerability reporting (Security tab), please -- not a
public issue. Most-valuable reports: the controller acting against user
intent, credential exposure, the metrics listener exposing more than
documented, or SHM permission bypasses.
