# Shared memory protocol (layout v1)

The daemon publishes a fixed-layout frame to `/dev/shm/zionstats-<uid>`
(mode 0600, uid-scoped). Any process of the same user may read it; readers
validate an ABI tag and never block the writer. Source of truth:
`crates/zionstats-core/src/shm/layout.rs` (compile-time size asserts).

## File geometry (262,144 bytes total)

| region | offset | size | count |
|---|---|---|---|
| header | 0 | 128 | 1 |
| gpu samples | 128 | 216 each | 8 |
| process entries | 1,856 | 408 each | 256 |
| control state | 106,304 | 144 | 1 |
| history rings | 106,448 | 6,152 each (8+256x6x4) | 8 |
| alert ring | 155,664 | 144 each | 64 |
| env block | 164,880 | 256 | 1 |
| reserved | 165,136..262,144 | -- | layout v1.x growth |

All structs are `#[repr(C)]`, zerocopy-checked, native-endian (same-host,
per-boot artifact). Readers validate `magic = "zionshm1"`,
`abi_tag = 0x01020304`, `layout_version = 1`, and `total_size` before use;
a mismatch is a loud error, never a misparse.

## Seqlock protocol

Single writer (the daemon, holding an advisory `flock` on the file so two
controllers can never clobber each other -- including the Python daemon):

1. `seqlock += 1` (odd, Relaxed)
2. `fence(SeqCst)`
3. write payload regions + counts
4. `seqlock += 1` (even, Release)

The writer-side SeqCst fence forbids payload writes from hoisting above
the odd store; the Release even store forbids payload writes from sinking
past it. Both fences are load-bearing: the multi-reader integrity test
caught torn frames before either was added (see `shm/mod.rs` comments).

Readers: load `seqlock` (Acquire); if odd, spin/yield; copy the frame;
`fence(SeqCst)`; reload -- retry up to 8 times. On exhaustion the reader
returns the last copy flagged `stale` (the TUI shows a STALE badge rather
than failing). Liveness: a daemon is considered down when its pid is gone
(`kill(pid, 0)`) or the newest GPU sample is older than 5 poll intervals.

## Header fields

`seqlock u64`, `daemon_pid u32`, `uid u32`, `start_ms u64`,
`tick_id u64` (publishes since start), `gpu_count`, `proc_count`,
`interval_ms`, `flags` (bit0 readonly, bit1 demo).

## GPU sample (216 B)

`ts_ms`, `uuid[48]`, `name[64]`, `mem_used/total u64`, `energy_mj u64`,
util + power + temp + fan as f32 (NaN = unsupported/unknown; the exporter
omits NaN series), `fan_pwm_set` (NaN while the driver owns fans),
clocks, PCIe rx/tx in bytes/s, `throttle_bits` (nvml.h ABI bitmask),
`pstate i8`, `verdict u8` (0 ok / 1 warn / 2 crit), validity `flags`.

## Process entry (408 B)

`command[256]`, `mem_used u64`, `pid`, `uid`, `sm/mem/enc/dec_pct` f32
(NaN = unavailable -- per-process utilization needs a raw-FFI follow-up),
`gpu_idx`, `kind` (C/G/C+G), flags (bit0 has container id, bit1 has pod
name), `container_id[16]` (short id), `pod_name[64]`, `user[32]`.
Container/pod attribution is resolved daemon-side from
`/proc/<pid>/cgroup` (v1+v2) with a PID-reuse-safe LRU.

## Versioning

Layout v1 is frozen: additive change requires bumping `layout_version`
(and the reader's accept list). Never reorder or shrink existing fields;
grow into the reserved tail.
