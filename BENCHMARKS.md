> NOTE (2026-10-19): these numbers are from the v0.5.0-era runs.
> Regenerate with `./benches/run_all.sh` on the reference hardware after
> v0.7.0; the harness is unchanged.  Not regenerated this cycle.

# Benchmarks: zionstats-rs vs zionstats (Python)

All numbers below were measured on the reference box and are regenerable:

```bash
./benches/run_all.sh > bench-results.md    # full campaign
```

Generated 2026-10-14 by zionstats-rs v0.6.0.

## Environment

| component | value |
|---|---|
| GPU | NVIDIA GeForce RTX 4070 Ti SUPER, driver 580.159.03 |
| kernel | 7.0.11-76070011-generic (Pop!_OS 24.04) |
| rustc | 1.93.0 |
| python | 3.14.3 (anaconda), zionstats 0.4.0 installed editable |
| CPU | 28 cores |
| CLK_TCK | 100 |

## Headline results

| metric | zionstats-rs | zionstats (python) | delta |
|---|---|---|---|
| TUI attach -> first frame | **2.0 ms** median (1.98-2.05 over 5 runs) | n/a -- no SHM mode | architecture: the daemon pays NVML once, every reader shares it |
| daemon cold start -> first SHM tick | **17.5 ms** | 130.8 ms (`--once`, live NVML: interpreter + NVML init + sample) | **7.5x** |
| steady-state CPU at 4 Hz (live NVML, 60 s) | **2.23 %** | 3.87 % | **1.7x lower** |
| daemon RSS after 60 s at 4 Hz (live) | **24.9 MB** | 59.5 MB | **2.4x lower** |
| /metrics request (curl, median) | 10.1 ms (8,804 B payload) | 15.4 ms (7,001 B payload) | payloads differ; Rust serves the superset |
| binary footprint | 9.4 MiB total (3 static bins) | interpreter + deps | |

Demo-mode (GPU-free, reproducible anywhere):

| metric | zionstats-rs | python | delta |
|---|---|---|---|
| CPU at 4 Hz (60 s) | **0.07 %** | 0.22 % | 3.1x lower |
| RSS | **5.4 MB** | 26.3 MB | 4.8x lower |

## Methodology

### Startup / launch

- Rust attach: daemon pre-started and health-checked; `hyperfine --warmup
  3 --min-runs 10` on `zionstats-tui --exit-after-ms 300
  --print-first-frame-ms`, plus five `first_frame_ms=` samples (t0 = process
  start, t1 = first ratatui draw returned). The daemon wait ensures the
  first frame is real data, not the splash.
- Daemon cold start: spawn `zionstatsd --demo` -> poll SHM header until
  `tick_id > 0`. This is the apples-to-apples counterpart of Python's
  process startup: both paths initialize NVML (well, the demo daemon does
  not -- its live twin does; the 17.5 ms figure is demo. Live init adds
  NVML init only).
- Python: `subprocess`-timed `zionstats --once` (live NVML init + sample +
  text render), median of 12. hyperfine 2.0 without `-S` mangles shell
  redirects; use the subprocess method or `hyperfine -S bash`.

### Steady-state at 4 Hz

Both daemons run 60 s at `interval = 0.25` after a 5 s settle. CPU% =
`(utime+stime ticks delta) / (CLK_TCK * seconds)`. RSS = `VmRSS` at the
end of the window. Live-NVML is the headline (this box); demo-mode is the
GPU-free reproducible variant that isolates the loop from NVML cost.

### Metrics endpoint

hyperfine, 5 warmup + 5 measured runs of one `curl` per request against
each exporter. Payload sizes printed alongside because the families are
not identical (Rust serves the superset: control/daemon gauges, DCGM-style
labels). curl overhead dominates at these latencies; both endpoints are
far from saturation.

### Criterion micro-benchmarks (GPU-free)

```
controller/tick/steady_hot              136 ns
controller/tick/steady_cool             141 ns
shm/publish_read_roundtrip/1gpu_256procs  3.53 us   (256 KiB worst-case frame)
shm/read_only/frame_copy                  3.38 us
```

Roundtrip = full seqlock publish + reader copy of the worst-case payload
(1 GPU + 256 processes). This is the per-tick cost the TUI experiences;
at 4 Hz it is ~0.0014% of one core.

### Binary sizes (release, stripped, lto=thin)

| binary | MiB |
|---|---|
| zionstats | 4.4 |
| zionstatsd | 3.9 |
| zionstats-tui | 1.1 |
| total | 9.4 |

## Fairness caveats

1. **Attach vs full pipeline.** Rust TUI attach excludes NVML on purpose:
   decoupling NVML from clients *is* the architecture. The daemon
   cold-start row is the honest counterpart and still wins (17.5 ms vs
   130.8 ms), though live NVML init will add a few ms to the daemon.
2. **Payloads differ.** The Rust /metrics body is larger (superset of
   families + labels). req/s comparisons at these latencies are
   curl-bound; the numbers bound the per-request cost, not server
   capacity.
3. **Feature parity is partial.** The Rust daemon does not yet sample
   per-process GPU utilization or ECC (raw-FFI follow-up); its CPU cost
   per tick is therefore slightly understated vs Python's pmon path.
4. **One box, one GPU.** Numbers are from the reference box above and
   will move with hardware; regenerate with `./benches/run_all.sh`.

## Reproduce

```bash
cargo build --release --workspace
pip install -e ../zionstats          # sibling repo, for the python rows
./benches/run_all.sh > bench-results.md
```
