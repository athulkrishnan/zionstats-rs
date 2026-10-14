#!/usr/bin/env bash
# C.1 startup/launch: TUI attach first-frame, Python --once/import, daemon cold-start
set -u
cd "$(dirname "$0")/.."
source benches/lib.sh
check_tools hyperfine python3 || exit 1

RS=./target/release/zionstats-tui
PY_ZIONSTATS="${PY_ZIONSTATS:-zionstats}"   # python entry point on PATH

echo "## Startup and launch"
echo
rm -f "$SHM_PATH"
(env ZION_CONFIG=/tmp/zs-bench.toml ./target/release/zionstatsd --demo -i 0.25 >/dev/null 2>&1 & echo $! > /tmp/zsd-bench.pid)
wait_first_tick || { echo "daemon failed to start"; exit 1; }

echo '```'
hyperfine --warmup 3 --min-runs 10 --style basic \
    "$RS --exit-after-ms 300 --print-first-frame-ms" 2>&1 | grep -E "Time|±|Benchmark"
echo '```'
echo
echo "first-frame samples (attach -> first draw, lower is better):"
for i in 1 2 3 4 5; do
    "$RS --exit-after-ms 200 --print-first-frame-ms" 2>/dev/null | tail -1
done
echo
echo '```'
hyperfine --warmup 3 --min-runs 10 --style basic "$PY_ZIONSTATS --demo --once >/dev/null" 2>&1 | grep -E "Time|±"
hyperfine --warmup 3 --min-runs 10 --style basic 'python3 -c "import zionstats.app"' 2>&1 | grep -E "Time|±"
echo '```'
echo
echo "daemon cold-start (spawn -> first SHM tick):"
T0=$(date +%s.%N)
rm -f "$SHM_PATH"
(env ZION_CONFIG=/tmp/zs-bench.toml ./target/release/zionstatsd --demo -i 0.25 >/dev/null 2>&1 & echo $! > /tmp/zsd-bench.pid)
wait_first_tick
T1=$(date +%s.%N)
awk -v a="$T0" -v b="$T1" 'BEGIN { printf "%.1f ms\n", (b - a) * 1000 }'
kill -TERM "$(cat /tmp/zsd-bench.pid)" 2>/dev/null; sleep 1; rm -f "$SHM_PATH"
# restart for any follow-on benches
(env ZION_CONFIG=/tmp/zs-bench.toml ./target/release/zionstatsd --demo -i 0.25 >/dev/null 2>&1 & echo $! > /tmp/zsd-bench.pid)
