#!/usr/bin/env bash
# shared helpers for the zionstats-rs benchmark scripts
set -u

CLK_TCK="${CLK_TCK:-$(getconf CLK_TCK)}"
SHM_PATH="/dev/shm/zionstats-$(id -u)"

check_tools() {
    local miss=0
    for t in "$@"; do
        command -v "$t" >/dev/null || { echo "MISSING TOOL: $t (install it to run benchmarks)"; miss=1; }
    done
    [ "$miss" = "0" ]
}

# cpu_ticks <pid> -> "utime stime"
cpu_ticks() { awk '{print $14, $15}' "/proc/$1/stat" 2>/dev/null; }

# cpu_percent <pid> <seconds> -> float
cpu_percent() {
    local pid=$1 secs=$2
    read -r u1 s1 <<<"$(cpu_ticks "$pid")"
    [ -z "${u1:-}" ] && { echo "ERR"; return; }
    sleep "$secs"
    read -r u2 s2 <<<"$(cpu_ticks "$pid")"
    [ -z "${u2:-}" ] && { echo "ERR"; return; }
    awk -v a=$((u1 + s1)) -v b=$((u2 + s2)) -v s="$secs" -v hz="$CLK_TCK" \
        'BEGIN { printf "%.2f", (b - a) / hz / s * 100 }'
}

# rss_kb <pid>
rss_kb() { awk '/^VmRSS:/ {print $2}' "/proc/$1/status" 2>/dev/null; }

wait_first_tick() {
    # <shm-reader-cmd args...>: poll header tick_id via python
    local deadline=$((SECONDS + 10))
    while [ $SECONDS -lt $deadline ]; do
        python3 - "$SHM_PATH" <<'PY' 2>/dev/null && return 0
import struct, sys
with open(sys.argv[1], "rb") as f:
    tick, = struct.unpack_from("<Q", f.read(56), 48)
sys.exit(0 if tick > 0 else 1)
PY
        sleep 0.2
    done
    return 1
}

md_table() { # md_table "title" (reads stdin rows: cells pipe-separated)
    echo; echo "### $1"; echo
    while IFS= read -r row; do
        [ -z "$row" ] && continue
        if [ -z "${header_done:-}" ]; then
            echo "| $row |"
            n=$(awk -F'|' '{print NF-2}' <<<"$row")
            printf '|%s|\n' "$(yes '---' | head -n "$n" | tr '\n' '|' | sed 's/|$//')"
            header_done=1
        else
            echo "| $row |"
        fi
    done
    unset header_done
}
