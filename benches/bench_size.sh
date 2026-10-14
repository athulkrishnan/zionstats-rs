#!/usr/bin/env bash
# C.6: binary footprint
set -u
cd "$(dirname "$0")/.."
md_table "Binary sizes (release, stripped)"
echo "binary | bytes | MiB"
for b in zionstats zionstatsd zionstats-tui; do
    f="target/release/$b"
    [ -f "$f" ] && awk -v n="$b" -v s="$(stat -c %s "$f")" 'BEGIN { printf "%s | %s | %.1f\n", n, s, s/1048576 }'
done
total=0
for b in zionstats zionstatsd zionstats-tui; do
    s=$(stat -c %s "target/release/$b" 2>/dev/null || echo 0)
    total=$((total + s))
done
awk -v t="$total" 'BEGIN { printf "total (3 bins) | %s | %.1f\n", t, t/1048576 }'
