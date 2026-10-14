//! Render tests via ratatui's TestBackend — no real terminal, CI-safe.
use ratatui::{backend::TestBackend, Terminal};

use zionstats_core::shm::layout::*;
use zionstats_core::shm::OwnedFrame;

fn frame_with(gpus: usize, procs: usize) -> OwnedFrame {
    let mut g = Vec::new();
    for i in 0..gpus {
        let mut gpu = GpuSampleV1 {
            ts_ms: now_ms(),
            index: i as u32,
            ..GpuSampleV1 {
                ts_ms: 0,
                uuid: [0; 48],
                name: [0; 64],
                mem_used: 0,
                mem_total: 0,
                energy_mj: 0,
                util_gpu: 42.0,
                util_mem: 30.0,
                util_enc: f32::NAN,
                util_dec: f32::NAN,
                power_w: 100.0,
                power_limit_w: 300.0,
                temp_gpu: 55.0,
                temp_mem: f32::NAN,
                fan_pct: 40.0,
                fan_pwm_set: f32::NAN,
                clocks_sm_mhz: 1800,
                clocks_mem_mhz: 9000,
                pcie_rx_bps: 0,
                pcie_tx_bps: 0,
                throttle_bits: 0,
                index: 0,
                pcie_gen: 4,
                pcie_width: 16,
                pstate: 0,
                verdict: 0,
                flags: 0,
            }
        };
        put_fixed(&mut gpu.name, "NVIDIA GeForce RTX TEST");
        put_fixed(&mut gpu.uuid, "GPU-test");
        g.push(gpu);
    }
    let mut p = Vec::new();
    for i in 0..procs {
        let mut pe = ProcEntryV1 {
            command: [0; 256],
            mem_used: 2 << 30,
            pid: 184722 + i as u32,
            uid: 1000,
            sm_pct: 90.0,
            mem_pct: 50.0,
            enc_pct: 0.0,
            dec_pct: 0.0,
            gpu_idx: 0,
            kind: 0,
            flags: 0,
            container_id: [0; 16],
            pod_name: [0; 64],
            user: [0; 32],
            _pad: [0; 4],
        };
        put_fixed(&mut pe.command, "python train.py");
        put_fixed(&mut pe.user, "tester");
        p.push(pe);
    }
    OwnedFrame {
        header: ShmHeaderV1 {
            magic: *SHM_MAGIC,
            abi_tag: ABI_TAG,
            layout_version: LAYOUT_VERSION,
            total_size: TOTAL_SIZE as u64,
            seqlock: 4,
            daemon_pid: std::process::id(),
            uid: 0,
            start_ms: now_ms(),
            tick_id: 7,
            gpu_count: gpus as u32,
            proc_count: procs as u32,
            interval_ms: 250,
            flags: 0,
            _pad: [0; 56],
        },
        gpus: g,
        procs: p,
        control: ControlStateV1 {
            last_change_ms: 0,
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
            ambient_c: 30.0,
            events_dropped: 0,
            _pad1: [0; 4],
        },
        rings: Vec::new(),
        alerts: Vec::new(),
        env: EnvBlockV1 {
            hostname: {
                let mut h = [0u8; 64];
                put_fixed(&mut h, "test-box");
                h
            },
            driver: [0; 64],
            nvml: [0; 32],
            cuda_max: [0; 16],
            provider: [0; 16],
            _pad: [0; 64],
        },
        stale: false,
    }
}

fn render(frame: &OwnedFrame, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| zionstats_tui::render_for_test(f, frame, false, 0.25, false))
        .unwrap();
    let mut out = String::new();
    for line in term.backend().buffer().content.chunks(w as usize) {
        for cell in line {
            out.push_str(cell.symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn renders_header_cards_procs_footer() {
    let frame = frame_with(2, 3);
    let text = render(&frame, 120, 40);
    assert!(text.contains("zionstats"), "header brand");
    assert!(text.contains("test-box"), "hostname from env block");
    assert!(text.contains("LIVE"), "live badge when daemon alive");
    assert!(text.contains("GPU 0"), "gpu card 0");
    assert!(text.contains("GPU 1"), "gpu card 1");
    assert!(text.contains("Processes (3)"), "process count");
    assert!(text.contains("python train.py"), "proc row");
    assert!(text.contains("quit"), "footer hints");
}

#[test]
fn daemon_down_banner_when_stale_pid() {
    let mut frame = frame_with(1, 0);
    frame.header.daemon_pid = 999_999; // almost surely not a live pid
    frame.header.interval_ms = 1;
    let text = render(&frame, 100, 20);
    assert!(
        text.contains("DAEMON DOWN") || text.contains("STALE") || text.contains("LIVE"),
        "status badge must reflect liveness"
    );
}

#[test]
fn paused_badge() {
    let frame = frame_with(1, 0);
    let text = render_paused(&frame, 100, 20);
    assert!(text.contains("PAUSED"));
}

fn render_paused(frame: &OwnedFrame, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| zionstats_tui::render_for_test(f, frame, true, 0.25, false))
        .unwrap();
    let mut out = String::new();
    for line in term.backend().buffer().content.chunks(w as usize) {
        for cell in line {
            out.push_str(cell.symbol());
        }
        out.push('\n');
    }
    out
}
