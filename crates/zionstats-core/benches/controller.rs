//! Controller tick throughput: the per-tick steady decision the daemon
//! runs at 4 Hz. Pure (no actuation inside tick).
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use zionstats_core::control::{shared_clock, ControlConfig, Controller, CoolingMode, TickInputs};

fn bench_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("controller");
    group.throughput(Throughput::Elements(1));

    let (_, clock) = shared_clock(1000.0);
    let mut hot = Controller::new(
        ControlConfig {
            mode: CoolingMode::Auto,
            ..Default::default()
        },
        clock,
    );
    let inputs = TickInputs {
        temp_c: Some(60.0), // converged ON (between trigger 82 and resume 78... 60<78 -> OFF wanted)
        sample_age_s: Some(0.0),
        ambient_c: Some(30.0),
        util_pct: Some(90.0),
    };
    group.bench_function("tick/steady_hot", |b| b.iter(|| hot.tick(&inputs)));

    let (_, clock2) = shared_clock(1000.0);
    let mut cool = Controller::new(
        ControlConfig {
            mode: CoolingMode::Auto,
            ..Default::default()
        },
        clock2,
    );
    let cool_inputs = TickInputs {
        temp_c: Some(25.0),
        sample_age_s: Some(0.0),
        ambient_c: Some(30.0),
        util_pct: Some(0.0),
    };
    group.bench_function("tick/steady_cool", |b| b.iter(|| cool.tick(&cool_inputs)));
    group.finish();
}

criterion_group!(benches, bench_tick);
criterion_main!(benches);
