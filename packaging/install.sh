#!/usr/bin/env bash
# zionstats v0.5 Rust binaries: build + install + systemd unit.
set -euo pipefail
PREFIX="${1:-/usr/local}"

cargo build --release --workspace
install -m 0755 target/release/zionstats "$PREFIX/bin/zionstats"
install -m 0755 target/release/zionstatsd "$PREFIX/bin/zionstatsd"
install -m 0755 target/release/zionstats-tui "$PREFIX/bin/zionstats-tui"

if [ -d /etc/systemd/system ]; then
    install -m 0644 packaging/systemd/zionstatsd.service /etc/systemd/system/
    mkdir -p /etc/zionstats
    [ -f /etc/zionstats/config.toml ] || cat > /etc/zionstats/config.toml <<'EOF'
# zionstats daemon config (same file schema as the Python TUI)
[daemon]
interval = 1.0        # poll seconds; floor 0.25 (4 Hz)
host = "127.0.0.1"
port = 9400
readonly = false
ambient_temp = 30.0   # static ambient (weather layer lands in v0.6)
# fan_control = false # opt-in direct GPU fan PWM (root; see README)
# fan_curve = [[40, 0], [50, 30], [70, 60], [82, 100]]

[cooling]
enabled = false       # smart-plug blower automation (same schema as Python)
mode = "off"

[[cooling.plugs]]
# backend = "shelly"  # shelly | tasmota (tuya/kasa stay on the Python daemon)
# name = "blower"
# host = "192.168.1.50"
EOF
    systemctl daemon-reload || true
    echo "unit installed: systemctl enable --now zionstatsd"
fi
echo "installed to $PREFIX/bin"
