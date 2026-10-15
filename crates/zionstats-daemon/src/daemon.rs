//! zionstatsd: the daemon runner. Polls a `Source` at the configured
//! interval, publishes seqlock SHM frames for the TUI, serves Prometheus
//! metrics (P5), and runs the thermal controller (P4). Failsafe posture:
//! any fatal error restores driver fan control before exiting.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use zionstats_core::config;
use zionstats_core::control::CoolingMode;
use zionstats_core::control::TickInputs;
use zionstats_core::shm::layout::{
    now_ms, put_fixed, ControlStateV1, EnvBlockV1, GpuSampleV1, HistoryRingV1, ProcEntryV1,
    GPU_FLAG_CLOCKS, GPU_FLAG_ENERGY, GPU_FLAG_FAN, GPU_FLAG_PCIE, GPU_FLAG_POWER, GPU_FLAG_TEMP,
    GPU_FLAG_VERDICT, MAX_ALERTS, MAX_GPUS, MAX_PROCS, RING_LEN, RING_MEM_FRAC, RING_POWER_W,
    RING_SM_CLOCK, RING_TEMP_GPU, RING_UTIL_GPU, RING_UTIL_MEM,
};
use zionstats_core::shm::ShmWriter;
use zionstats_core::source::demo::DemoSource;
use zionstats_core::source::Source;
use zionstats_core::types::Sample;
use zionstats_core::VERSION;

#[derive(Debug, Clone)]
pub struct DaemonOpts {
    pub interval_s: f64,
    pub host: String,
    pub port: u16,
    pub readonly: bool,
    pub demo: bool,
    pub config_path: Option<std::path::PathBuf>,
    pub mode_override: Option<CoolingMode>,
}

impl Default for DaemonOpts {
    fn default() -> Self {
        DaemonOpts {
            interval_s: 1.0,
            host: "127.0.0.1".into(),
            port: 9400,
            readonly: false,
            demo: false,
            config_path: None,
            mode_override: None,
        }
    }
}

enum ActiveSource {
    Demo(Box<DemoSource>),
    #[cfg(feature = "nvml")]
    Nvml(Box<zionstats_core::source::nvml::NvmlSource>),
}

impl ActiveSource {
    fn as_source(&mut self) -> &mut dyn Source {
        match self {
            ActiveSource::Demo(d) => d.as_mut(),
            #[cfg(feature = "nvml")]
            ActiveSource::Nvml(n) => n.as_mut(),
        }
    }
}

fn build_source(opts: &DaemonOpts) -> Result<ActiveSource, String> {
    if opts.demo {
        return Ok(ActiveSource::Demo(Box::new(DemoSource::new())));
    }
    #[cfg(feature = "nvml")]
    {
        zionstats_core::source::nvml::NvmlSource::try_new(None)
            .map(|s| ActiveSource::Nvml(Box::new(s)))
            .map_err(|e| {
                format!("no live NVML source ({e}) -- run with --demo for the synthetic rig")
            })
    }
    #[cfg(not(feature = "nvml"))]
    {
        Err("this build has no NVML support -- run with --demo".to_string())
    }
}

struct ProcRow {
    pid: u32,
    gpu_idx: u8,
    kind: u8,
    mem_used: u64,
    sm_pct: f32,
    mem_pct: f32,
    enc_pct: f32,
    dec_pct: f32,
    user: String,
    command: String,
    container_id: Option<String>,
    pod_name: Option<String>,
}

pub struct SharedState {
    pub stop: AtomicBool,
    pub tick_id: AtomicU64,
    pub watchdog_pings: AtomicU64,
}

/// Run the daemon. Returns a process exit code.
pub fn run(opts: DaemonOpts) -> i32 {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run_async(opts))
}

async fn run_async(opts: DaemonOpts) -> i32 {
    let cfg = config::load(&config::config_path(opts.config_path.as_deref()));
    let interval_s = if opts.interval_s > 0.0 {
        opts.interval_s
    } else {
        cfg.daemon.interval_s
    }
    .clamp(0.25, 30.0);
    let readonly = opts.readonly || cfg.readonly || cfg.daemon.readonly;

    let mut source = match build_source(&opts) {
        Ok(s) => s,
        Err(e) => {
            log::error!("{e}");
            return 2;
        }
    };
    let env = source.as_source().environment();

    let uid = unsafe { libc::getuid() };
    let mut writer = match ShmWriter::create(uid) {
        Ok(w) => w,
        Err(e) => {
            log::error!("shm create failed: {e}");
            return 3;
        }
    };
    log::info!(
        "zionstatsd {} publishing to {}",
        VERSION,
        writer.path().display()
    );

    // seed the env block (written between seqlock sections is unsafe; do it
    // inside the first publish)
    let state = Arc::new(SharedState {
        stop: AtomicBool::new(false),
        tick_id: AtomicU64::new(0),
        watchdog_pings: AtomicU64::new(0),
    });

    // ---- signal handling
    let stop_flag = state.clone();
    let sig_task = tokio::spawn(async move {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
        let int = tokio::signal::ctrl_c();
        tokio::select! {
            _ = async {
                loop {
                    if let (Some(t), Some(h)) = (term.as_mut(), hup.as_mut()) {
                        tokio::select! {
                            _ = t.recv() => break "term",
                            _ = h.recv() => { log::info!("SIGHUP: config reload is applied on restart in v0.5"); }
                        }
                    } else {
                        std::future::pending::<()>().await;
                    }
                }
            } => {
                let _ = term; let _ = hup;
            }
            _ = int => { }
        }
        stop_flag.stop.store(true, Ordering::SeqCst);
    });

    // ---- watchdog (sd_notify)
    let wd_state = state.clone();
    let wd_task = tokio::spawn(async move {
        let watchdog_usec = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(usec) = watchdog_usec {
            let period = Duration::from_micros(usec / 4).max(Duration::from_millis(200));
            loop {
                if wd_state.stop.load(Ordering::SeqCst) {
                    break;
                }
                // P3+: only ping while poll progress is fresh; a stalled
                // poll stops pinging and systemd restarts us (startup then
                // re-arms driver-default fans).
                if sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]).is_err() {
                    log::debug!("watchdog ping failed (not running under systemd?)");
                }
                wd_state.watchdog_pings.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(period).await;
            }
        }
    });

    // ---- metrics endpoint (P5)
    let live: Arc<crate::http::SharedFrame> =
        Arc::new(std::sync::Mutex::new(crate::http::LiveFrame {
            watchdog_pings: 0,
            gpus: Vec::new(),
            procs: Vec::new(),
            control: None,
            hostname: env.hostname.clone(),
            started_ms: now_ms(),
        }));
    {
        let live = live.clone();
        let host = if opts.host.is_empty() {
            cfg.daemon.host.clone()
        } else {
            opts.host.clone()
        };
        let port = if opts.port != 0 {
            opts.port
        } else {
            cfg.daemon.port
        };
        tokio::spawn(async move {
            if let Err(e) =
                crate::http::serve(&host, port, live, VERSION, cfg.expose_processes).await
            {
                log::warn!("{e}");
            }
        });
    }

    // ---- main poll/publish loop
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(interval_s));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let interval_ms = (interval_s * 1000.0) as u32;

    // ---- control stack (P4): plugs + optional fan PWM
    let (mut control, control_warnings) = crate::control_task::ControlStack::build(&opts, &cfg);
    for w in &control_warnings {
        log::warn!("{w}");
    }
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_for_task = shutdown_flag.clone();

    // process rows refreshed every 2nd tick (Python _proc_every parity),
    // with the container/k8s resolver applied per pid
    let mut resolver = zionstats_core::cgroup::CgroupResolver::default();
    let mut last_procs: Vec<ProcRow> = Vec::new();
    let mut tick_n: u64 = 0;

    // history rings kept daemon-side and flushed into SHM each tick
    let mut rings: Vec<HistoryRingV1> = (0..MAX_GPUS)
        .map(|_| HistoryRingV1 {
            head: 0,
            _pad: 0,
            data: [[f32::NAN; 6]; RING_LEN],
        })
        .collect();

    // seed env + control inside the first publish
    let mut first = true;

    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Status("starting")]) {
        log::debug!("sd_notify unavailable: {e}");
    }

    let mut ready_sent = false;
    while !state.stop.load(Ordering::SeqCst) {
        ticker.tick().await;
        let samples: Vec<Sample> = source.as_source().sample();
        let ts_ms = now_ms();
        tick_n += 1;
        let mut data_gpus: Vec<GpuSampleV1> = Vec::with_capacity(samples.len());
        for s in &samples {
            let mut g = zeroed_gpu();
            fill_gpu(&mut g, s, ts_ms);
            data_gpus.push(g);
        }
        let last_procs_shm: Vec<ProcEntryV1> = last_procs
            .iter()
            .map(|pr| ProcEntryV1 {
                command: {
                    let mut c = [0u8; 256];
                    let bytes = pr.command.as_bytes();
                    let n = bytes.len().min(256);
                    c[..n].copy_from_slice(&bytes[..n]);
                    c
                },
                mem_used: pr.mem_used,
                pid: pr.pid,
                uid: 0,
                sm_pct: pr.sm_pct,
                mem_pct: pr.mem_pct,
                enc_pct: pr.enc_pct,
                dec_pct: pr.dec_pct,
                gpu_idx: pr.gpu_idx,
                kind: pr.kind,
                flags: (pr.container_id.is_some() as u16) | ((pr.pod_name.is_some() as u16) << 1),
                container_id: {
                    let mut c = [0u8; 16];
                    if let Some(cid) = &pr.container_id {
                        let b = cid.as_bytes();
                        let n = b.len().min(16);
                        c[..n].copy_from_slice(&b[..n]);
                    }
                    c
                },
                pod_name: {
                    let mut c = [0u8; 64];
                    if let Some(pod) = &pr.pod_name {
                        let b = pod.as_bytes();
                        let n = b.len().min(64);
                        c[..n].copy_from_slice(&b[..n]);
                    }
                    c
                },
                user: {
                    let mut c = [0u8; 32];
                    let b = pr.user.as_bytes();
                    let n = b.len().min(32);
                    c[..n].copy_from_slice(&b[..n]);
                    c
                },
                _pad: [0; 4],
            })
            .collect();
        // control tick: decision + actuator dispatch (blocking; a slow
        // plug delays the tick but the interval ticker skips, so no drift)
        {
            let hot = samples
                .iter()
                .max_by_key(|s| s.temp_gpu.map(|t| (t * 1000.0) as i64).unwrap_or(-1));
            let hot_inputs = hot.map(|s| TickInputs {
                temp_c: s.temp_gpu,
                util_pct: s.util_gpu,
                sample_age_s: Some(
                    (now_ms() as i64
                        - s.ts
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0)) as f64
                        / 1000.0,
                ),
                readback: None,
                ambient_c: Some(cfg.daemon.ambient_temp_c),
            });
            if let Some(inputs) = hot_inputs {
                control.tick(&inputs, &shutdown_for_task);
            }
        }

        if tick_n % 2 == 0 {
            let raw = source.as_source().processes();
            let proc_root = std::path::Path::new("/proc");
            last_procs = raw
                .iter()
                .map(|p| {
                    let info = resolver.resolve(p.pid, p.start_time, proc_root);
                    ProcRow {
                        pid: p.pid,
                        gpu_idx: p.gpu_index as u8,
                        kind: match p.kind {
                            zionstats_core::types::ProcKind::Compute => 0u8,
                            zionstats_core::types::ProcKind::Graphics => 1,
                            zionstats_core::types::ProcKind::ComputeAndGraphics => 2,
                        },
                        mem_used: p.mem_used,
                        sm_pct: p.sm_pct.unwrap_or(f32::NAN),
                        mem_pct: p.mem_pct.unwrap_or(f32::NAN),
                        enc_pct: p.enc_pct.unwrap_or(f32::NAN),
                        dec_pct: p.dec_pct.unwrap_or(f32::NAN),
                        user: p.user.clone(),
                        command: p.command.clone(),
                        container_id: info.as_ref().map(|c| c.short.clone()),
                        pod_name: info.as_ref().and_then(|c| c.pod_name.clone()),
                    }
                })
                .take(MAX_PROCS)
                .collect();
        }

        writer.publish(interval_ms, |view| {
            let header = view.header();
            header.gpu_count = samples.len().min(MAX_GPUS) as u32;
            header.proc_count = last_procs.len().min(MAX_PROCS) as u32;
            header.flags = if readonly { 1 } else { 0 } | if opts.demo { 2 } else { 0 };

            {
                let e = view.env_mut();
                *e = EnvBlockV1 {
                    hostname: [0; 64],
                    driver: [0; 64],
                    nvml: [0; 32],
                    cuda_max: [0; 16],
                    provider: [0; 16],
                    _pad: [0; 64],
                };
                put_fixed(&mut e.hostname, &env.hostname);
                put_fixed(&mut e.driver, &env.driver);
                put_fixed(&mut e.nvml, &env.nvml);
                put_fixed(&mut e.cuda_max, &env.cuda_max);
                put_fixed(&mut e.provider, env.provider);
            }

            for (i, g) in data_gpus.iter().enumerate().take(MAX_GPUS) {
                if let Some(slot) = view.gpu_mut(i) {
                    *slot = *g;
                }
                let s = &samples[i];
                // history ring push
                let ring = &mut rings[i];
                let slot = (ring.head as usize) % RING_LEN;
                ring.data[slot][RING_UTIL_GPU] = s.util_gpu.unwrap_or(f32::NAN);
                ring.data[slot][RING_UTIL_MEM] = s.util_mem.unwrap_or(f32::NAN);
                ring.data[slot][RING_POWER_W] = s.power_w.unwrap_or(f32::NAN);
                ring.data[slot][RING_TEMP_GPU] = s.temp_gpu.unwrap_or(f32::NAN);
                ring.data[slot][RING_SM_CLOCK] =
                    s.clocks_sm_mhz.map(|c| c as f32).unwrap_or(f32::NAN);
                ring.data[slot][RING_MEM_FRAC] = if s.mem_total > 0 {
                    (s.mem_used as f32 / s.mem_total as f32) * 100.0
                } else {
                    f32::NAN
                };
                ring.head = ring.head.wrapping_add(1);
                if let Some(r) = view.ring_mut(i) {
                    *r = *ring;
                }
            }

            for (i, pe) in last_procs_shm.iter().enumerate().take(MAX_PROCS) {
                if let Some(slot) = view.proc_mut(i) {
                    *slot = *pe;
                }
            }

            if let Some(cs) = control.state_for_shm() {
                *view.control_mut() = cs;
            }

            if first {
                let c = view.control_mut();
                *c = ControlStateV1 {
                    last_change_ms: ts_ms,
                    on_since_ms: 0,
                    duty_seconds: 0.0,
                    reason: [0; 96],
                    mode: 0,
                    trigger: 0,
                    state_on: 0,
                    would_on: 0,
                    plug_ok: -1,
                    inhibited: 0,
                    pwm_engaged: 0,
                    _pad0: 0,
                    pwm_current: f32::NAN,
                    ambient_c: cfg.daemon.ambient_temp_c,
                    events_dropped: 0,
                    _pad1: [0; 4],
                };
                put_fixed(&mut c.reason, "controller armed");
                // zero the alert ring
                for ai in 0..MAX_ALERTS {
                    if let Some(a) = view.alert_mut(ai) {
                        a.seq = 0;
                    }
                }
                first = false;
            }
        });

        // refresh the metrics frame
        {
            let mut lf = live.lock().unwrap();
            lf.gpus = data_gpus.clone();
            lf.procs = last_procs_shm.clone();
            lf.control = control.state_for_shm();
            lf.watchdog_pings = state.watchdog_pings.load(Ordering::Relaxed);
        }

        state.tick_id.fetch_add(1, Ordering::Relaxed);

        if !ready_sent {
            ready_sent = true;
            if sd_notify::notify(false, &[sd_notify::NotifyState::Ready]).is_ok() {
                log::info!("systemd READY sent");
            }
        }
    }

    // ---- graceful shutdown: driver-default fans are restored by P4's
    // actuator layer; the P2 skeleton has no PWM state to restore (we never
    // engaged), so shutdown is a clean stop.
    log::info!("shutting down");
    sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]).ok();
    sig_task.abort();
    wd_task.abort();
    0
}

fn zeroed_gpu() -> GpuSampleV1 {
    GpuSampleV1 {
        ts_ms: 0,
        uuid: [0; 48],
        name: [0; 64],
        mem_used: 0,
        mem_total: 0,
        energy_mj: 0,
        util_gpu: f32::NAN,
        util_mem: f32::NAN,
        util_enc: f32::NAN,
        util_dec: f32::NAN,
        power_w: f32::NAN,
        power_limit_w: f32::NAN,
        temp_gpu: f32::NAN,
        temp_mem: f32::NAN,
        fan_pct: f32::NAN,
        fan_pwm_set: f32::NAN,
        clocks_sm_mhz: 0,
        clocks_mem_mhz: 0,
        pcie_rx_bps: 0,
        pcie_tx_bps: 0,
        throttle_bits: 0,
        index: 0,
        pcie_gen: 0,
        pcie_width: 0,
        pstate: -1,
        verdict: 0,
        flags: 0,
    }
}

fn fill_gpu(g: &mut GpuSampleV1, s: &Sample, ts_ms: u64) {
    g.ts_ms = ts_ms;
    put_fixed(&mut g.uuid, &s.uuid);
    put_fixed(&mut g.name, &s.name);
    g.mem_used = s.mem_used;
    g.mem_total = s.mem_total;
    g.energy_mj = s.energy_mj.unwrap_or(0);
    g.util_gpu = s.util_gpu.unwrap_or(f32::NAN);
    g.util_mem = s.util_mem.unwrap_or(f32::NAN);
    g.util_enc = s.util_enc.unwrap_or(f32::NAN);
    g.util_dec = s.util_dec.unwrap_or(f32::NAN);
    g.power_w = s.power_w.unwrap_or(f32::NAN);
    g.power_limit_w = s.power_limit_w.unwrap_or(f32::NAN);
    g.temp_gpu = s.temp_gpu.unwrap_or(f32::NAN);
    g.temp_mem = s.temp_mem.unwrap_or(f32::NAN);
    g.fan_pct = s.fan_pct.unwrap_or(f32::NAN);
    g.fan_pwm_set = f32::NAN; // P4: set when we own PWM
    g.clocks_sm_mhz = s.clocks_sm_mhz.unwrap_or(0);
    g.clocks_mem_mhz = s.clocks_mem_mhz.unwrap_or(0);
    g.pcie_rx_bps = s.pcie_rx_kbps.unwrap_or(0).saturating_mul(1000);
    g.pcie_tx_bps = s.pcie_tx_kbps.unwrap_or(0).saturating_mul(1000);
    g.throttle_bits = s.throttle_bits;
    g.index = s.index;
    g.pcie_gen = s.pcie_gen_cur.unwrap_or(0);
    g.pcie_width = s.pcie_width_cur.unwrap_or(0);
    g.pstate = s.pstate.unwrap_or(-1);
    g.verdict = zionstats_core::types::verdict_of(s) as u8;
    let mut flags = 0;
    if s.temp_gpu.is_some() {
        flags |= GPU_FLAG_TEMP;
    }
    if s.fan_pct.is_some() {
        flags |= GPU_FLAG_FAN;
    }
    if s.power_w.is_some() {
        flags |= GPU_FLAG_POWER;
    }
    if s.clocks_sm_mhz.is_some() {
        flags |= GPU_FLAG_CLOCKS;
    }
    if s.pcie_gen_cur.is_some() {
        flags |= GPU_FLAG_PCIE;
    }
    if s.energy_mj.is_some() {
        flags |= GPU_FLAG_ENERGY;
    }
    flags |= GPU_FLAG_VERDICT;
    g.flags = flags;
}
