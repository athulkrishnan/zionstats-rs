#!/usr/bin/env bash
# Full benchmark campaign -> markdown on stdout (redirect to a file).
#   ./benches/run_all.sh > bench-results.md
set -u
cd "$(dirname "$0")/.."
source benches/lib.sh

echo "# zionstats-rs benchmark results"
echo
echo "generated $(date -u '+%Y-%m-%d %H:%M UTC') by $(./target/release/zionstats --version 2>/dev/null || echo 'zionstats-rs v?')"
echo
echo "## Environment"
echo
echo "| component | value |"
echo "|---|---|"
GPU=$(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null | head -1 || echo "none detected")
echo "| GPU | $GPU |"
echo "| kernel | $(uname -r) |"
echo "| rustc | $(rustc --version) |"
echo "| python | $(python3 --version) |"
echo "| CPU cores | $(nproc) |"
echo "| CLK_TCK | $CLK_TCK |"

echo
echo "## Criterion micro-benchmarks (zionstats-core)"
echo '```'
cargo bench -p zionstats-core 2>&1 | grep -E "time:|roundtrip|tick/|read_only"
echo '```'

./benches/bench_size.sh
./benches/bench_startup.sh
./benches/bench_http.sh
if command -v nvidia-smi >/dev/null; then
    ./benches/bench_daemon_load.sh --live 60
else
    echo "SKIPPED live-NVML load bench (nvidia-smi not found)"
fi
./benches/bench_daemon_load.sh --demo 60
echo
echo "Reproduce with: ./benches/run_all.sh > bench-results.md"
