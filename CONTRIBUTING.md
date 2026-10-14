# Contributing to zionstats-rs

## Development loop

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check           # licenses + advisories
cargo build --release
zionstats --demo           # full stack, no GPU needed
```

All four checks must pass. CI runs exactly these.

## Rules

- **GPU-free tests**: `cargo test --workspace` must pass on a runner with
  no NVIDIA driver. NVML code lives behind the `nvml` feature; tests use
  `MockSource`/`DemoSource`.
- **Fail-safe posture is not optional**: changes to the controller or
  actuators keep dry-run default, readback verify, stale force-off, and
  the min on/off windows, and come with table tests (see
  `crates/zionstats-core/src/control/mod.rs` tests; injectable clock).
- **Performance**: criterion benches live in
  `crates/zionstats-core/benches/`. A regression over ~20% on
  `shm/*` or `controller/*` needs justification in the PR.
- **Layout v1 is frozen**: additive SHM changes bump `layout_version` and
  extend, never reorder (see docs/SHM.md).
- **No new runtime dependencies** without discussion; everything in
  `deny.toml`'s license allowlist only (MIT/Apache/BSD/ISC/Unicode/Zlib).
- **Docs are emoji-free** and written in a professional register.

## Benchmarks

```bash
cargo bench -p zionstats-core        # criterion micro
./benches/run_all.sh > bench-results.md   # full campaign (needs hyperfine)
```

Please attach before/after bench output for performance-relevant PRs.

## Reporting issues

Security issues: see SECURITY.md (private disclosure). Everything else:
GitHub issues with the output of `zionstatsd --check-config` (it prints
no secrets) and, for bugs, a `--demo` reproduction when possible.
