#!/usr/bin/env bash
# C.4: metrics endpoint req/s (curl-per-request, both exporters running)
set -u
cd "$(dirname "$0")/.."
source benches/lib.sh
check_tools hyperfine curl || exit 1
PY_LOG="${PY_LOG:-zionstats}"

rm -f /tmp/zs-bench.jsonl
"$PY_LOG" --demo --log /tmp/zs-bench.jsonl --export 127.0.0.1:9835 -i 0.25 >/dev/null 2>&1 &
echo $! > /tmp/zspy-bench.pid
sleep 3

BYTES_R=$(curl -s http://127.0.0.1:9400/metrics -o /dev/null -w '%{size_download}')
BYTES_P=$(curl -s http://127.0.0.1:9835/metrics -o /dev/null -w '%{size_download}')

md_table "Metrics endpoint (curl per request)"
echo "endpoint | req/s (hyperfine median) | payload bytes"
echo '```'
echo "rust /metrics :9400:"
hyperfine --warmup 5 --min-runs 5 --style basic 'curl -s http://127.0.0.1:9400/metrics -o /dev/null' 2>&1 | grep -E "Time|±"
echo "  payload: $BYTES_R bytes"
echo "python /metrics :9835:"
hyperfine --warmup 5 --min-runs 5 --style basic 'curl -s http://127.0.0.1:9835/metrics -o /dev/null' 2>&1 | grep -E "Time|±"
echo "  payload: $BYTES_P bytes"
echo '```'

kill -TERM "$(cat /tmp/zspy-bench.pid)" 2>/dev/null; rm -f /tmp/zs-bench.jsonl
