# Plug backends — worldwide guide (v0.7.0)

zionstats drives smart plugs LOCALLY on your LAN (no cloud account sees
your telemetry; automation keeps working when the internet is down).
This guide covers every backend the Rust daemon speaks, what each device
class can and cannot report, and the verification status of every row.

Safety first, at the bottom.  Mains voltage is involved; read it.

## Backends

| backend | transport | on/off | status readback | `power()` metering |
|---|---|---|---|---|
| shelly (Gen1 REST) | HTTP | yes | yes | yes (`relay.power`, W) |
| shelly (Gen2/Plus RPC) | HTTP | yes | yes | yes (`Switch.GetStatus.power`) |
| tasmota | HTTP | yes | yes | yes (`StatusSNS.Energy.Power`) |
| tuya (v3.3 local) | TCP :6668 | yes | dps "1"/"20" | only a recognized metering DP |
| tuya (v3.4 static-AES-GCM) | TCP :6668 | yes | dps "1"/"20" | same caveat |
| kasa | TCP :9999 | yes | `relay_state` | `emeter.get_realtime` (KP115/KP125M-class) |
| ha (Home Assistant) | REST | yes | `state == "on"` | `attributes.power` / `current_power_w` |

Backend notes:

- **Shelly** — Gen1 devices answer `GET /relay/{ch}`; Gen2/Gen3 (Plus
  series) speak the RPC API (`Switch.Set`, `Switch.GetStatus`).  Gen3
  compatibility mode uses the Gen2 RPC shape.  No SDK needed.
- **Tasmota** — stock HTTP command endpoint (`/cm?cmnd=...`).  flashed
  Sonoff S31 (US) and S26 (EU) are the common hosts; see the Tasmota
  docs for flashing prerequisites (not covered here).
- **Tuya** — local-LAN control needs the device `id` and `local_key`
  (one-time `python -m tinytuya wizard` from the Python sibling's docs
  covers pairing).  v3.4 ships the static AES-GCM variant; tinytuya's
  session-negotiation v3.4 (HMAC trailer) is NOT implemented -- devices
  that require it report no readback and are skipped with a warning.
  `power()` returns watts ONLY when the firmware exposes a DP under a
  name the client recognizes; DP ids vary wildly across firmwares and
  an unrecognized scale is never guessed (Python parity).
- **Kasa** — raw TCP :9999 with the IOT autokey XOR protocol.  Metering
  models (KP115, KP125M, KP400-class) answer `emeter.get_realtime`;
  switch-only models (HS103/HS105) have no meter -- `power()` is None,
  which is honest, not a bug.
- **Home Assistant** — the hub route: any switch entity behind HA
  (including Zigbee/Z-Wave plugs HA can see) works through the REST API
  with a long-lived token.  Metering depends on the underlying entity's
  attributes.

## Config

```toml
[[cooling.plugs]]
backend = "shelly"          # shelly | tasmota | tuya | kasa | ha
name = "blower"             # your label; shows up in /metrics as plug="blower"
host = "192.168.1.50"       # shelly/tasmota/kasa
gen = 2                     # shelly only: 1 = Gen1 REST, 2 = Gen2 RPC
channel = 0                 # shelly channel

[[cooling.plugs]]
backend = "tuya"
name = "blower"
id = "<device id>"
key = "<local key>"
address = "192.168.1.51"    # without it the plug is skipped with a warning
version = 3.4               # 3.3 or 3.4

[[cooling.plugs]]
backend = "kasa"
name = "heater"
host = "192.168.1.52"

[[cooling.plugs]]
backend = "ha"
name = "blower"
base_url = "http://ha.lan:8123"
token = "<long-lived access token>"
entity_id = "switch.blower"
```

## Verified-against table

Rows are filled ONLY from proven runs.  `mock-proven` = protocol-level
CI tests against an in-process mock server speaking the verified wire
format (Tuya constants cross-checked against tinytuya 1.20.0 source;
Kasa XOR key 171 against python-kasa 0.11.0.1).  `hardware-verified` =
a real device, on a dated run.  Nothing in this table may claim
hardware verification without a real run behind it.

| backend | on/off | status | power() | status |
|---|---|---|---|---|
| shelly Gen1 | mock-proven | mock-proven | mock-proven | -- |
| shelly Gen2 | mock-proven | mock-proven | mock-proven | -- |
| tasmota | mock-proven | mock-proven | mock-proven | -- |
| tuya v3.3 | mock-proven | mock-proven | mock-proven (DP-dependent) | -- |
| tuya v3.4 (static GCM) | mock-proven | mock-proven | mock-proven (DP-dependent) | -- |
| kasa | mock-proven | mock-proven | mock-proven | -- |
| ha | mock-proven | mock-proven | mock-proven | -- |

Hardware-verified rows: none in this release cycle.  When you verify a
real device, add the row with the date and the firmware you tested.

## Region / model notes

- **Shelly**: Plug S Gen1 (EU, 12 A), Plug US Gen1/Gen2 (15 A/120 V),
  Plug S MTR Gen3 (16 A, overheat protection).  Community testing puts
  Shelly idle-accuracy ahead of Kasa for whole-rig metering.
- **Kasa**: KP115 / KP125M meter; HS103/HS105 switch only (no meter).
- **Tasmota**: Sonoff S31 (US 15 A), S26 (EU 10 A).
- **Tuya**: the Zebronics ZEB-SP110 (10 A, IN) is the driver behind
  this backend; whitelabels of the same Tuya modules exist worldwide --
  local-key pairing is identical everywhere (iot.tuya.com access varies
  by region, the wizard docs cover the fallbacks).

## Safety

- Mains voltage kills.  If you are not comfortable wiring/verifying a
  plug, stop here.
- Ship with `mode = "off"` (the default), verify with `dry_run`, and
  only then arm `auto`.
- Respect the plug's rating: a 10 A plug is 2.3 kW at 230 V / 1.2 kW at
  120 V.  A single blower is fine; a space heater plus a rig is not.
  No daisy-chaining power strips.
- The controller fails safe: stale GPU telemetry or stale weather
  forces the plug OFF (auto+temp), every command is verified by
  readback (retry x3), and `--readonly` builds no actuators at all.
- Community power figures for context (not zionstats measurements):
  an RTX 3090 rig is comfortable on a 250-300 W power limit, a 4090 at
  80% PL trades ~18% watts for ~5% performance, and a 5090 wants a cap
  >= 480 W of its 575 W stock draw.  Whole-rig wall metering via a
  metering plug beats GPU-sensor accounting because it includes CPU,
  RAM, and fans.
