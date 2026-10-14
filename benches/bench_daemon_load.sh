#!/usr/bin/env bash
# C.2+C.3: steady-state CPU% + RSS at 4 Hz. Usage: --live | --demo [--seconds 60]
set -u
cd "$(dirname "$0")/.."
source benches/lib.sh
MODE="${1:---demo}"; SECONDS_RUN="${2:-60}"
LIVE=""
[ "$MODE" = "--live" ] && LIVE=""
PY_LOG="${PY_LOG:-zionstats}"

rm -f "$SHM_PATH" /tmp/zs-bench.jsonl
if [ "$MODE" = "--live" ]; then
    (env ZION_CONFIG=/tmp/zs-bench.toml ./target/release/zionstatsd -i 0.25 >/dev/null 2>&1 & echo $! > /tmp/zsd-bench.pid)
    "$PY_LOG" --log /tmp/zs-bench.jsonl -i 0.25 >/dev/null 2>&1 &
    echo $! > /tmp/zspy-bench.pid
else
    (env ZION_CONFIG=/tmp/zs-bench.toml ./target/release/zionstatsd --demo -i 0.25 >/dev/null 2>&1 & echo $! > /tmp/zsd-bench.pid)
    "$PY_LOG" --demo --log /tmp/zs-bench.jsonl -i 0.25 >/dev/null 2>&1 &
    echo $! > /tmp/zspy-bench.pid
fi
sleep 5  # settle

RUST_PID=$(cat /tmp/zsd-bench.pid); PY_PID=$(cat /tmp/zspy-bench.pid)
R_CPU=$(cpu_percent "$RUST_PID" "$SECONDS_RUN"); R_RSS=$(rss_kb "$RUST_PID")
P_CPU=$(cpu_percent "$PY_PID" "$SECONDS_RUN"); P_RSS=$(rss_kb "$PY_PID")

LABEL="$MODE"
md_table "Steady-state at 4 Hz ($MODE, ${SECONDS_RUN}s window)"
echo "metric | zionstats-rs | zionstats (python) | ratio"
echo "CPU % | ${R_CPU}% | ${P_CPU}% | $(awk -v r="$R_CPU" -v p="$P_CPU" 'BEGIN { if (p+0>0) printf "%.1fx lower", p/r; else "n/a" }')"
echo "RSS | ${R_RSS:-?} KB | ${P_RSS:-?} KB | $(awk -v r="${R_RSS:-0}" -v p="${P_RSS:-0}" 'BEGIN { if (r+0>0) printf "%.1fx lower", p/r; else "n/a" }')"

kill -TERM "$RUST_PID" 2>/dev/null; kill -TERM "$PY_PID" 2>/dev/null
rm -f "$SHM_PATH" /tmp/zs-bench.jsonl
