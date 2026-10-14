//! SHM micro-benchmarks: publish->read roundtrip and read-only copy,
//! through the real writer/reader path. GPU-free; criterion harness.
use criterion::{criterion_group, criterion_main, Criterion};
use zionstats_core::shm::layout::*;
use zionstats_core::shm::{ShmReader, ShmWriter};

fn bench_roundtrip(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench-shm");
    let mut writer = ShmWriter::create_at(&path).unwrap();
    let reader = ShmReader::attach_path(&path).unwrap();

    // seed one GPU row + a full process table (worst-case payload)
    writer.publish(250, |v| {
        if let Some(g) = v.gpu_mut(0) {
            fill_gpu(g, 0);
        }
        for i in 0..MAX_PROCS {
            if let Some(p) = v.proc_mut(i) {
                fill_proc(p, i);
            }
        }
        v.header().gpu_count = 1;
        v.header().proc_count = MAX_PROCS as u32;
    });

    let mut group = c.benchmark_group("shm");
    group.throughput(criterion::Throughput::Bytes(TOTAL_SIZE as u64));
    group.bench_function("publish_read_roundtrip/1gpu_256procs", |b| {
        b.iter(|| {
            writer.publish(250, |v| {
                if let Some(g) = v.gpu_mut(0) {
                    g.ts_ms = now_ms();
                }
            });
            reader.read().unwrap()
        })
    });
    group.bench_function("read_only/frame_copy", |b| {
        b.iter(|| reader.read().unwrap())
    });
    group.finish();
}

fn fill_gpu(g: &mut GpuSampleV1, i: usize) {
    g.ts_ms = now_ms();
    g.index = i as u32;
    g.util_gpu = 42.0;
    g.temp_gpu = 55.0;
    g.power_w = 120.0;
    g.mem_used = 2 << 30;
    g.mem_total = 16 << 30;
    put_fixed(&mut g.name, "NVIDIA GeForce RTX BENCH");
}

fn fill_proc(p: &mut ProcEntryV1, i: usize) {
    *p = ProcEntryV1 {
        command: [0; 256],
        mem_used: 13 << 30,
        pid: 100_000 + i as u32,
        uid: 1000,
        sm_pct: 90.0,
        mem_pct: 50.0,
        enc_pct: 0.0,
        dec_pct: 0.0,
        gpu_idx: (i % 8) as u8,
        kind: 0,
        flags: 0,
        container_id: [0; 16],
        pod_name: [0; 64],
        user: [0; 32],
        _pad: [0; 4],
    };
    put_fixed(&mut p.command, "python train.py --batch 256 --amp");
    put_fixed(&mut p.user, "bench");
}

criterion_group!(benches, bench_roundtrip);
criterion_main!(benches);
