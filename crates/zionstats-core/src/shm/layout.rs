//! SHM layout v2 — byte-exact structures shared between the daemon
//! (writer) and any reader (TUI, scripts). `/dev/shm/zionstats-<uid>`,
//! mode 0600, 256 KiB total. Native endianness: this is a same-host,
//! per-boot artifact; `abi_tag` is written natively and validated on
//! attach so any mismatch fails loudly instead of misparsing.
//!
//! v2 (this version): GPU_STRIDE 216 -> 256, appending `ecc_sbe`/`ecc_dbe`
//! (u64 volatile ECC counters), a `vendor` byte, and padding to the
//! stride. All later region offsets are derived and shift automatically;
//! readers validate `layout_version == 2` and reject older files loudly.
//!
//! NaN f32 = unsupported/unknown (mirrors Python `None`; the exporter
//! omits NaN series entirely).

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const SHM_MAGIC: &[u8; 8] = b"zionshm1";
pub const ABI_TAG: u32 = 0x0102_0304;
pub const LAYOUT_VERSION: u32 = 2;

pub const MAX_GPUS: usize = 8;
pub const MAX_PROCS: usize = 256;
pub const RING_LEN: usize = 256;
pub const RING_METRICS: usize = 6;
pub const MAX_ALERTS: usize = 64;

// region offsets (compile-time checked below)
pub const HDR_SIZE: usize = 128;
pub const GPUS_OFF: usize = HDR_SIZE;
pub const GPU_STRIDE: usize = 256;
pub const PROCS_OFF: usize = GPUS_OFF + MAX_GPUS * GPU_STRIDE;
pub const PROC_STRIDE: usize = 408;
pub const CTRL_OFF: usize = PROCS_OFF + MAX_PROCS * PROC_STRIDE;
pub const CTRL_SIZE: usize = 144;
pub const RINGS_OFF: usize = CTRL_OFF + CTRL_SIZE;
pub const RING_SIZE: usize = 8 + RING_LEN * RING_METRICS * 4;
pub const ALERTS_OFF: usize = RINGS_OFF + MAX_GPUS * RING_SIZE;
pub const ALERT_SIZE: usize = 144;
pub const ENV_OFF: usize = ALERTS_OFF + MAX_ALERTS * ALERT_SIZE;
pub const ENV_SIZE: usize = 256;
pub const TOTAL_SIZE: usize = 262_144; // 256 KiB (payload ~165 KiB, rest reserved)

/// History-ring metric slot order (fixed discriminants; layout v2 may
/// extend but must not reorder).
pub const RING_UTIL_GPU: usize = 0;
pub const RING_UTIL_MEM: usize = 1;
pub const RING_POWER_W: usize = 2;
pub const RING_TEMP_GPU: usize = 3;
pub const RING_SM_CLOCK: usize = 4;
pub const RING_MEM_FRAC: usize = 5;

// header flags
pub const FLAG_READONLY: u32 = 1 << 0;
pub const FLAG_DEMO: u32 = 1 << 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ShmHeaderV1 {
    pub magic: [u8; 8],
    pub abi_tag: u32,
    pub layout_version: u32,
    pub total_size: u64,
    /// seqlock: odd = writer inside section; accessed via AtomicU64.
    pub seqlock: u64,
    pub daemon_pid: u32,
    pub uid: u32,
    pub start_ms: u64,
    pub tick_id: u64,
    pub gpu_count: u32,
    pub proc_count: u32,
    pub interval_ms: u32,
    pub flags: u32,
    pub _pad: [u8; 56],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct GpuSampleV1 {
    pub ts_ms: u64,
    pub uuid: [u8; 48],
    pub name: [u8; 64],
    pub mem_used: u64,
    pub mem_total: u64,
    pub energy_mj: u64,
    pub util_gpu: f32,
    pub util_mem: f32,
    pub util_enc: f32,
    pub util_dec: f32,
    pub power_w: f32,
    pub power_limit_w: f32,
    pub temp_gpu: f32,
    pub temp_mem: f32,
    pub fan_pct: f32,
    /// last PWM we commanded; NaN = driver default owns
    pub fan_pwm_set: f32,
    pub clocks_sm_mhz: u32,
    pub clocks_mem_mhz: u32,
    /// bytes/s (converted from NVML KB/s)
    pub pcie_rx_bps: u32,
    pub pcie_tx_bps: u32,
    pub throttle_bits: u32,
    pub index: u32,
    pub pcie_gen: u8,
    pub pcie_width: u8,
    /// -1 unknown, else P-state number
    pub pstate: i8,
    /// Verdict::Ok=0 Warn=1 Crit=2
    pub verdict: u8,
    /// validity bitmask: bit0 temp, bit1 fan, bit2 power, bit3 clocks,
    /// bit4 pcie, bit5 energy, bit6 verdict, bit7 ecc
    pub flags: u32,
    /// volatile ECC since driver load; u64::MAX = unsupported (v2)
    pub ecc_sbe: u64,
    pub ecc_dbe: u64,
    /// 0 unknown/nvidia, 1 amd, 2 intel, 3 demo (v2)
    pub vendor: u8,
    pub _pad: [u8; 23],
}

pub const GPU_FLAG_TEMP: u32 = 1 << 0;
pub const GPU_FLAG_FAN: u32 = 1 << 1;
pub const GPU_FLAG_POWER: u32 = 1 << 2;
pub const GPU_FLAG_CLOCKS: u32 = 1 << 3;
pub const GPU_FLAG_PCIE: u32 = 1 << 4;
pub const GPU_FLAG_ENERGY: u32 = 1 << 5;
pub const GPU_FLAG_VERDICT: u32 = 1 << 6;
pub const GPU_FLAG_ECC: u32 = 1 << 7;

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ProcEntryV1 {
    /// NUL-padded cmdline (embedded NULs -> spaces)
    pub command: [u8; 256],
    pub mem_used: u64,
    pub pid: u32,
    pub uid: u32,
    pub sm_pct: f32,
    pub mem_pct: f32,
    pub enc_pct: f32,
    pub dec_pct: f32,
    pub gpu_idx: u8,
    /// 0=C 1=G 2=C+G
    pub kind: u8,
    /// bit0 has_container, bit1 has_pod
    pub flags: u16,
    /// 12-char short id + NULs (full id kept daemon-side for metrics)
    pub container_id: [u8; 16],
    pub pod_name: [u8; 64],
    pub user: [u8; 32],
    pub _pad: [u8; 4],
}

pub const PROC_FLAG_HAS_CONTAINER: u16 = 1 << 0;
pub const PROC_FLAG_HAS_POD: u16 = 1 << 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ControlStateV1 {
    pub last_change_ms: u64,
    /// 0 = not on
    pub on_since_ms: u64,
    pub duty_seconds: f64,
    pub reason: [u8; 96],
    /// 0=off 1=dry_run 2=auto 3=manual
    pub mode: u8,
    /// 0=temp 1=usage
    pub trigger: u8,
    pub state_on: u8,
    pub would_on: u8,
    /// -1 unknown 0 fail 1 ok
    pub plug_ok: i8,
    pub inhibited: u8,
    /// true while we own fan PWM
    pub pwm_engaged: u8,
    pub _pad0: u8,
    pub pwm_current: f32,
    pub ambient_c: f32,
    pub events_dropped: u32,
    pub _pad1: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct HistoryRingV1 {
    /// monotonic counter; slot = head % RING_LEN
    pub head: u32,
    pub _pad: u32,
    /// [metric][sample]; NAN = no sample yet
    pub data: [[f32; RING_METRICS]; RING_LEN],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct AlertV1 {
    pub seq: u64,
    pub ts_ms: u64,
    pub msg: [u8; 120],
    /// 0=info 1=warn 2=crit
    pub level: u8,
    /// ControlEventKind as u8 (see core::control::EventKind order)
    pub kind: u8,
    pub _pad: [u8; 6],
}

/// Environment strings for the reader's header line (TUI, /snapshot).
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct EnvBlockV1 {
    pub hostname: [u8; 64],
    pub driver: [u8; 64],
    pub nvml: [u8; 32],
    pub cuda_max: [u8; 16],
    pub provider: [u8; 16],
    pub _pad: [u8; 64],
}

const _: () = assert!(size_of::<ShmHeaderV1>() == HDR_SIZE);
const _: () = assert!(size_of::<GpuSampleV1>() == GPU_STRIDE);
const _: () = assert!(size_of::<ProcEntryV1>() == PROC_STRIDE);
const _: () = assert!(size_of::<ControlStateV1>() == CTRL_SIZE);
const _: () = assert!(size_of::<HistoryRingV1>() == RING_SIZE);
const _: () = assert!(size_of::<AlertV1>() == ALERT_SIZE);
const _: () = assert!(size_of::<EnvBlockV1>() == ENV_SIZE);
const _: () = assert!(ENV_OFF + ENV_SIZE <= TOTAL_SIZE);
const _: () = assert!(TOTAL_SIZE == 262_144);

/// NUL-padded copy helper (truncate silently; entries are sized generously).
pub fn put_fixed(dst: &mut [u8], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    dst[n..].fill(0);
}

/// Read a NUL-padded fixed field as a lossy String.
pub fn get_fixed(src: &[u8]) -> String {
    let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
    String::from_utf8_lossy(&src[..end]).into_owned()
}

/// Now in unix milliseconds (monotonic-enough wall clock for SHM stamps).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

use std::mem::size_of;
