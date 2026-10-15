# zionstats-rs

[![CI](https://github.com/athulkrishnan/zionstats-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/athulkrishnan/zionstats-rs/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-linux-blue)](README.md#platform-support)

**btop-grade NVIDIA GPU observability in Rust: a 4 Hz always-on daemon, a
shared-memory TUI that attaches in milliseconds, and embedded Prometheus
metrics -- in three small static binaries.**

zionstats-rs is the Rust core of [zionstats](https://github.com/athulkrishnan/zionstats)
(the Python GPU monitor). The two are sibling projects sharing one config
file, one metric-family vocabulary, and one design philosophy: local-first,
fail-safe, insight-first.

```text
 Zionstats-rs architecture
  ┌────────────────────┐   seqlock SHM    ┌──────────────────┐
  │  zionstatsd        │ ───────────────▶ │ zionstats-tui    │
  │  NVML @ 4 Hz       │  /dev/shm/…      │ attaches in ms   │
  │  fan/plug control  │  (0600, flock)   │ never inits NVML │
  │  Prometheus :9400  │                  └──────────────────┘
  └────────────────────┘   any number of readers, they never block
```

## Try it in one command

```bash
cargo build --release
./target/release/zionstats --demo
```

![zionstats-rs TUI monitoring an Ollama llama-server workload](assets/zionstats-ollama.gif)

Built for the local-LLM rig: the process table resolves `llama-server` /
`vllm` to real VRAM + SM% utilization, the daemon exports Prometheus
metrics for your fleet, and thermal automation keeps inference cool.
See [docs/MONITORING-OLLAMA.md](docs/MONITORING-OLLAMA.md).

`--demo` spawns an embedded synthetic two-GPU daemon, waits for its first
frame, attaches the TUI, and tears everything down when you quit -- no GPU
required. (Screenshot/asciinema placeholder: `zionstats --demo` under
tmux, 120x34.)

## Why we rewrote the core in Rust

The Python monitor grew a real job: 24/7 headless thermal protection. That
meant a daemon that polls NVML at 4 Hz forever, a UI that can attach and
detach without disturbing it, and no interpreter in the hot path. The
numbers below are from [BENCHMARKS.md](BENCHMARKS.md) (methodology and
reproduce commands there; RTX 4070 Ti SUPER, kernel 7.0.11):

| metric | zionstats-rs | zionstats (python) | note |
|---|---|---|---|
| TUI attach -> first frame | see BENCHMARKS.md | n/a (no SHM mode) | the daemon already paid the NVML cost once, every reader shares it |
| daemon cold start -> first tick | see BENCHMARKS.md | ~1 s process start | apples-to-apples: both init NVML |
| steady CPU at 4 Hz | see BENCHMARKS.md | see BENCHMARKS.md | live NVML, 60 s window |
| daemon RSS | see BENCHMARKS.md | see BENCHMARKS.md | |
| SHM publish+read roundtrip | ~4 us | n/a | 256 KiB worst-case frame, criterion |
| controller tick | ~140 ns | n/a | pure state machine |
| install footprint | 3 static binaries | interpreter + deps (or a PyInstaller bundle) | |

**What Python still does better** (and stays fully supported):
mature TUI polish (16 themes, options menu, mouse, presets), record/replay
traces, the climate/weather layer, and Tuya/Kasa/Home-Assistant plug
backends. The Rust daemon speaks Shelly and Tasmota natively today and
refuses to fight the Python daemon for the same plug (flock).

**Safety posture (inherited, not relaxed):** fan PWM and plug actuation
ship off; the cooling controller is a line-by-line port of the Python
fail-safe state machine (hysteresis, min on/off, duty cap, stale force-off,
readback verify x3); SIGTERM restores driver fan control; `--readonly`
builds no actuators at all.

## Quickstart

From source (any Linux with the NVIDIA driver for live metrics):

```bash
cargo build --release
sudo ./packaging/install.sh           # bins to /usr/local + systemd unit
sudo systemctl enable --now zionstatsd
zionstats                             # TUI attaches to the daemon
curl 127.0.0.1:9400/metrics           # Prometheus
```

Or grab a static musl build from Releases (no interpreter needed).

### CLI

| command | what it does |
|---|---|
| `zionstats` | TUI: attaches to the running daemon |
| `zionstats --demo` | one-command demo (embedded daemon, auto-cleanup) |
| `zionstats --daemon` | run the daemon in the foreground |
| `zionstatsd` | daemon entry (what systemd runs) |
| `zionstatsd --check-config` | print resolved config + warnings; never touches NVML |
| `zionstats completions bash\|zsh\|fish` | shell completions |
| `zionstats-tui --interval 0.25` | TUI refresh at 4 Hz |

Keys: `q` quit, `space` pause, `+/-` rate, `s` sort processes.

## Configuration

Same file as the Python project: `~/.config/zionstats/config.toml`
(`$ZION_CONFIG` overrides). The Rust side reads it and never writes it.

| key | default | what |
|---|---|---|
| `[daemon] interval` | 1.0 | poll seconds, floor 0.25 (4 Hz) |
| `[daemon] host / port` | 127.0.0.1 / 9400 | metrics socket |
| `[daemon] readonly` | false | build no actuators at all |
| `[daemon] fan_control` | false | opt-in GPU fan PWM (root; driver restored on exit) |
| `[daemon] fan_curve` | (40,0)(50,30)(70,60)(82,100) | temp C -> PWM % |
| `[daemon] ambient_temp` | 30.0 | ambient for the temp trigger (weather layer lands in v0.6) |
| `[daemon] shutdown_plug_policy` | auto_off | what SIGTERM does to an ON plug |
| `[cooling] *` | see [zionstats config](https://github.com/athulkrishnan/zionstats#readme) | same schema as Python |
| `[[cooling.plugs]] backend` | -- | `shelly` (gen 1/2), `tasmota`; `tuya`/`kasa`/`ha` stay on the Python daemon |
| `[export] expose_processes` | true | PIDs/users/cmdlines in `/metrics` (see SECURITY.md) |

## Metrics

`/metrics` serves the same `zionstats_gpu_*` / `zionstats_process_*`
families as the Python exporter -- an existing Grafana board keeps
working -- plus DCGM-style labels (`device`, `modelName`, `Hostname`),
`zionstats_control_*`, and `zionstats_daemon_*` gauges. OpenMetrics
negotiation via `Accept`; `/snapshot` returns live JSON; `/healthz`
reports tick age.

## Shared memory

The daemon publishes a fixed-layout, seqlock-protected frame to
`/dev/shm/zionstats-<uid>` (mode 0600). Readers validate an ABI tag and
never block the writer. Byte map and protocol:
[docs/SHM.md](docs/SHM.md). Architecture:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Compatibility matrix

| feature | zionstats-rs | zionstats (python) |
|---|---|---|
| live NVML telemetry, 4 Hz | yes | yes |
| headless daemon (systemd notify + watchdog) | yes | no (`--log`) |
| SHM with N non-blocking readers | yes | no |
| instant TUI attach (no NVML init) | yes | n/a |
| Prometheus + snapshot | yes (:9400) | yes (:9835) |
| fan PWM control | yes (opt-in) | no |
| Shelly / Tasmota plugs | yes | yes |
| Tuya / Kasa / HA plugs | no (use Python daemon) | yes |
| themes, mouse, presets, options menu | no | yes |
| record / replay traces | no | yes |
| climate / weather layer | no | yes |
| container + k8s process attribution | yes | no |

## Platform support

Linux x86_64 / aarch64 (static musl release builds). Needs the NVIDIA
driver for live metrics; `zionstats --demo` works anywhere.

## Roadmap

- v0.6.x: SIGHUP hot-reload, per-process GPU utilization via raw NVML FFI,
  ECC counters, man pages in release zips
- v0.7: theme port, record/replay against SHM, Tuya bridge
- See the Python repo for everything the mature TUI already does

## Sibling project

[zionstats](https://github.com/athulkrishnan/zionstats) -- the original
Python monitor: full TUI polish, climate-aware cooling, Tuya support, and
the development home of the fail-safe controller design this crate ports.

## Credits

Ideas, not code: [btop](https://github.com/aristocratos/btop) and
[nvitop](https://github.com/XuehaiPan/nvitop) for the TUI concept (the
Python TUI ports btop's Apache-2.0 graph engine; this Rust repo contains
no third-party source), [nvtop](https://github.com/Syllo/nvtop) for panel
concepts, [nvitop-exporter](https://github.com/XuehaiPan/nvitop) for the
embedded-endpoint idea (metric families kept drop-in compatible), and
DCGM's label style. See [NOTICE](NOTICE).

## License

Apache-2.0 -- see [LICENSE](LICENSE).
