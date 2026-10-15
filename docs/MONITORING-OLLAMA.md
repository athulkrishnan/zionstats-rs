# Monitoring Ollama, llama.cpp and vLLM with zionstats

zionstats shows you what your GPU is actually doing while a local LLM
runs: per-process VRAM, SM utilization of the inference process, power
draw, clocks, PCIe throughput, and thermal headroom -- in a btop-style
TUI, exported to Prometheus, with optional thermal automation.

## Quickstart (Ollama already installed)

```bash
# daemon (systemd or foreground) + TUI, both from this repo
zionstatsd                     # polls NVML at 4 Hz, publishes SHM + :9400/metrics
zionstats-tui                  # attaches in milliseconds -- never touches NVML itself
```

That is it. Start a chat in Ollama and watch:

- the header shows total VRAM across all GPUs (`ΣMEM 15.0/16.0GiB`)
- the process table shows `llama-server` with its GPU memory and SM%
- the detail panel (press `d`) graphs utilization, power, temperature,
  SM clocks and PCIe throughput with thermal-threshold lines
- the verdict chip tells you the bottleneck: `COMPUTE-BOUND`,
  `MEMORY-BOUND`, `POWER-LIMITED`, `THERMALLY-LIMITED`, or `IDLE`

## Reading the metrics during inference

Local LLM inference is **memory-bandwidth-bound**: on a GeForce card the
SM utilization sits at 25-40% while the memory subsystem runs near
saturation. That is normal -- token generation streams model weights, it
does not do dense math. Key indicators:

| symptom | verdict chip | what it means |
|---|---|---|
| tokens/s drops, power pinned at limit | POWER-LIMITED | card hit its power cap; undervolt or accept it |
| temps near `temp_slowdown` (usually 90C) | THERMALLY-LIMITED | clocks are being cut -- improve airflow |
| util high, power low, temps fine | MEMORY-BOUND | bandwidth-limited workload; a smaller quant will not help throughput |
| VRAM at 94%+ | memory pressure | a second model or a larger context will OOM |

## Process-level attribution

The process table resolves every GPU process to a user and a command
line:

```
PID      GPU  MEM      SM%   COMMAND
184722   0    13.4Gi   97    python train.py --batch 256 --amp
192004   1    73.5Gi   88    python -m vllm.entrypoints serve
792344   0    14.6Gi    9    /usr/local/lib/ollama/llama-server --model ...
```

Press `v` for the tree view (which parent spawned the GPU process --
useful for telling a jupyter kernel from a stray benchmark), `space` to
tag processes, `enter` for the full detail screen (environment, cwd,
live per-process graphs), `T`/`K` to terminate a runaway process after
a typed confirmation.

## Prometheus + Grafana (headless)

Run the exporter without any TUI:

```bash
zionstats --log gpu.jsonl --export 127.0.0.1:9835
# Rust daemon: zionstatsd --export 127.0.0.1:9400
```

Scrape either endpoint; import the shipped Grafana dashboard
(`dashboards/zionstats-prom.json`) for utilization, VRAM, power-vs-limit,
temperature, clocks, PCIe, energy and a top-GPU-processes table. Metric
families are `zionstats_gpu_*` / `zionstats_process_*` -- the same names
from both the Python and Rust implementations.

## Thermal automation (optional)

If your rig has an external blower on a smart plug, zionstats can drive
it: ON when GPU utilization crosses a threshold, OFF after the card
idles and cools. Fail-safe by construction -- dry-run mode first,
readback verification, stale-data force-offs. See the README's climate
section and `zionstats --plug-test` to verify hardware safely.

## Notes per runtime

- **Ollama**: the process appears as `llama-server`; VRAM stays reserved
  between prompts (model cached in memory) -- that is Ollama's loader,
  not a leak. Utilization drops to 0 between tokens.
- **llama.cpp**: appears as `llama-cli` / `server`; same reading as
  Ollama. Multiple slots show as one process.
- **vLLM**: appears as the `vllm` serve process, typically pinned near
  100% of the reserved VRAM; on multi-GPU setups use `--only` /
  layout modes per card, and the Grafana dashboard for the fleet view.
