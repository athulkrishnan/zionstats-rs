# Changelog

## v0.7.0 -- roadmap parity, worldwide plugs, same honesty (2026-10-19)

### Added
- **Tuya local control** (v3.3 + static-v3.4 AES-GCM), **Kasa** (raw TCP
  autokey-XOR), and **Home Assistant** plug backends -- wire formats
  verified against tinytuya 1.20.0 and python-kasa 0.11.0.1 sources,
  pinned by in-process mock-server tests.  Plug wall-power `power()`
  lands on every backend with per-plug session Wh and
  `zionstats_plug_*` exporter families; the TUI control row shows the
  wall watts.
- **Per-process GPU utilization** via the raw
  `nvmlDeviceGetProcessUtilization` FFI (binding already in
  nvml-wrapper-sys; GPU-free merge tests behind a trait seam).
- **Volatile ECC counters** (layout v2: GPU_STRIDE 216 -> 256,
  ecc_sbe/ecc_dbe + vendor byte; readers reject v1 loudly).
- **Insights layer** -- Python insights.py port (session Wh with the mJ
  wrap guard, avg/peak/idle-leak W, PCIe saturation, thermal trend) in
  a new SHM v2 insight region + exporter families.
- **Climate layer** -- Open-Meteo provider with a disk cache shared
  with the Python stack, Stull wet-bulb physics, and the reserved
  stale-weather failsafe branch activated (auto+temp only).
- **SIGHUP hot-reload** with fail-safe stack rebuild: fans restore
  exactly once via Drop, plugs hear nothing, belief re-syncs by
  readback.
- **Periodic plug readback reconciliation** wired into the daemon
  (readback_period_s config).
- **Record/replay** -- Python-compatible frame shape (schema-tested),
  .gz via flate2, `zionstats --replay` embedded daemon, REPLAY badge;
  cross-compat fixture recorded live from the sibling.
- **16 themes + live picker** (port of themes.py; quantizer golden-
  tested against Python outputs), `theme` config key.
- **uid -> username** resolution from /etc/passwd (cached, negative-
  cached, numeric fallback).
- **AMD source** (amdgpu sysfs + shared DRM fdinfo per-process stats,
  shared fixtures with the sibling) and an **experimental Intel source**
  (i915/xe; power honestly always None); `--provider` /
  `[daemon] provider` selection with auto-fallback.
- **Man pages** for all three binaries (installed + shipped in release
  zips); docs/PLUGS.md worldwide plug guide.

### Changed
- SHM layout v2 (see docs/SHM.md); every later region offset shifts
  automatically.  `/metrics` gpu series gains an additive vendor label;
  the demo grows to four synthetic GPUs.
- The seqlock stress test now guarantees reader progress under load
  (start barrier + readers drain at least one check).

### Notes
- BENCHMARKS.md numbers are from v0.5.0-era runs -- regenerate on the
  reference hardware after v0.7.0 (not done in this cycle; the bench
  harness is unchanged).
- Hardware verification rows in docs/PLUGS.md remain mock-proven only
  this cycle.


## v0.6.0 -- standalone carve-out + calibration (2026-10-14)

First standalone release, carved from the
[zionstats](https://github.com/athulkrishnan/zionstats) monorepo at v0.5.0.

### Added
- **One-command demo**: `zionstats --demo` spawns an embedded demo daemon
  (process_group + PDEATHSIG, zero orphans) and attaches the TUI.
- **TUI calibration**: deadline-driven redraw at `--interval` (true 4 Hz at
  0.25 s; was ~3.7 fps under a 250 ms sleep floor), 60 Hz input, process
  sort (`s`), cooling status row, `--print-first-frame-ms` bench flag
  (attach -> first draw ~2 ms release).
- **CLI polish**: `zionstats completions bash|zsh|fish`,
  `zionstatsd --check-config` (pure introspection, exit 1 on invalid TOML),
  `zionstats_daemon_watchdog_pings_total` metric.
- Criterion benches (shm roundtrip, controller tick) + `benches/run_all.sh`
  campaign scripts -> BENCHMARKS.md.
- CI: fmt/clippy/test/deny/release-build; release: musl amd64/arm64 zips
  with checksums; packaging/install.sh + systemd unit.

### Fixed
- Workspace dependency warnings (default-features now declared at the
  workspace table; unified bin requests nvml explicitly, TUI never links it).
- Unified bin TUI dispatch (stub from the monorepo copy).
