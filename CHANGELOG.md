# Changelog

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
