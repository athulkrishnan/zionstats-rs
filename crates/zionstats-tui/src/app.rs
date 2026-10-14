//! Ephemeral SHM-backed terminal UI. Attaches to `/dev/shm/zionstats-<uid>`
//! and renders from there; NVML is never initialized in this process
//! (verified by the nvml-absent test). ratatui's Buffer diffing produces
//! minimal escape sequences (flicker-free by construction).

use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, LineGauge, Paragraph, Sparkline};
use ratatui::Frame;
use ratatui::Terminal;

use zionstats_core::shm::layout::{get_fixed, AlertV1, GpuSampleV1, ProcEntryV1};
use zionstats_core::shm::{OwnedFrame, ShmReader};

pub struct TuiOpts {
    pub interval_s: f64,
    pub readonly: bool,
    /// hidden: exit after N ms (launch benchmark)
    pub exit_after_ms: Option<u64>,
    /// hidden: count draw calls (bench output)
    pub run_once_frame: bool,
}

pub fn run(opts: TuiOpts) -> i32 {
    let uid = unsafe { libc::getuid() };
    let reader = match ShmReader::attach(uid) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "zionstats-tui: cannot attach shared memory: {e}\n\
                 start the daemon first:  zionstatsd --demo   (or zionstats --daemon)"
            );
            return 2;
        }
    };

    crossterm::terminal::enable_raw_mode().ok();
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen).ok();
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>> =
        Terminal::new(backend).ok();

    let started = Instant::now();
    let mut interval = opts.interval_s.clamp(0.25, 30.0);
    let mut paused = false;
    let mut frames = 0u64;
    let mut last_frame: Option<OwnedFrame> = None;
    let exit_code = loop {
        // read + draw
        if !paused {
            if let Ok(frame) = reader.read() {
                last_frame = Some(frame);
            }
        }
        if let (Some(term), Some(frame)) = (terminal.as_mut(), last_frame.as_ref()) {
            let _ = term.draw(|f| ui(f, frame, paused, interval, opts.readonly));
            frames += 1;
        }
        if let Some(ms) = opts.exit_after_ms {
            if started.elapsed() >= Duration::from_millis(ms) {
                break 0; // bench run
            }
        }
        if opts.run_once_frame {
            break 0;
        }
        // input poll at ~16ms for snappy keys
        if poll_key(Duration::from_millis(16), &mut interval, &mut paused) {
            break 0;
        }
        let target = Duration::from_secs_f64(interval);
        let elapsed = started.elapsed();
        let _ = elapsed; // pacing handled by poll timeout below
        std::thread::sleep(target.min(Duration::from_millis(250)));
    };

    if let Some(mut term) = terminal {
        let _ = term.clear();
    }
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )
    .ok();
    crossterm::terminal::disable_raw_mode().ok();
    if opts.exit_after_ms.is_some() || opts.run_once_frame {
        // bench mode: machine-readable stats on stdout
        println!(
            "frames={frames} elapsed_ms={:.1} fps={:.1}",
            started.elapsed().as_secs_f64() * 1000.0,
            frames as f64 / started.elapsed().as_secs_f64().max(0.000001)
        );
    }
    exit_code
}

/// returns true when the app should quit
fn poll_key(timeout: Duration, interval: &mut f64, paused: &mut bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
            if let crossterm::event::Event::Key(k) = crossterm::event::read().unwrap_or_else(|_| {
                crossterm::event::Event::Key(crossterm::event::KeyEvent::from(
                    crossterm::event::KeyCode::Null,
                ))
            }) {
                if k.kind == crossterm::event::KeyEventKind::Press {
                    use crossterm::event::KeyCode;
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return true,
                        KeyCode::Char(' ') | KeyCode::Char('p') => *paused = !*paused,
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            *interval = (*interval * 1.5).min(30.0);
                        }
                        KeyCode::Char('-') => *interval = (*interval / 1.5).max(0.25),
                        _ => {}
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

/// Test hook: render one frame into an arbitrary backend (TestBackend).
pub fn render_for_test(
    f: &mut Frame,
    frame: &OwnedFrame,
    paused: bool,
    interval: f64,
    readonly: bool,
) {
    ui(f, frame, paused, interval, readonly)
}

fn ui(f: &mut Frame, frame: &OwnedFrame, paused: bool, interval: f64, readonly: bool) {
    let alive = frame.daemon_alive();
    let [header, mut body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let _ = &mut body;

    // ---- header
    let env_host = get_fixed(&frame.env.hostname);
    let driver = get_fixed(&frame.env.driver);
    let flags = frame.header.flags;
    let mode_tag: String = if flags & 2 != 0 {
        "demo".to_string()
    } else {
        get_fixed(&frame.env.provider)
    };
    let status = if !alive {
        Span::styled(
            " DAEMON DOWN ",
            Style::default().bg(Color::Red).fg(Color::White),
        )
    } else if frame.stale {
        Span::styled(
            " STALE ",
            Style::default().bg(Color::Yellow).fg(Color::Black),
        )
    } else if paused {
        Span::styled(
            " PAUSED ",
            Style::default().bg(Color::Blue).fg(Color::White),
        )
    } else {
        Span::styled(" LIVE ", Style::default().bg(Color::Green).fg(Color::Black))
    };
    let ro = if readonly || flags & 1 != 0 {
        " RO"
    } else {
        ""
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "zionstats",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {env_host} [{mode_tag}] {driver}{ro} ")),
            status,
            Span::raw(format!(" {:.1}s", interval)),
        ])),
        header,
    );

    // ---- body: per-GPU cards + process table + alerts
    let n = frame.gpus.len();
    if n == 0 {
        f.render_widget(
            Paragraph::new("waiting for first frame from zionstatsd..."),
            body,
        );
    } else {
        let alerts: Vec<&AlertV1> = frame.alerts.iter().rev().take(2).collect();
        let rows = if alerts.is_empty() { 0 } else { alerts.len() };
        let [cards, procs, alert_rows] = Layout::vertical([
            Constraint::Length((n as u16).clamp(1, 8) * 4),
            Constraint::Min(6),
            Constraint::Length(rows as u16),
        ])
        .areas(body);
        let card_chunks = Layout::vertical(
            (0..n as u16)
                .map(|_| Constraint::Length(4))
                .collect::<Vec<_>>(),
        )
        .split(cards);
        for (i, g) in frame.gpus.iter().enumerate() {
            render_gpu_card(f, card_chunks[i], g, frame.rings.get(i));
        }
        render_procs(f, procs, &frame.procs);
        if !alerts.is_empty() {
            let lines: Vec<Line> = alerts
                .iter()
                .map(|a| {
                    let style = match a.level {
                        2 => Style::default().fg(Color::Red),
                        1 => Style::default().fg(Color::Yellow),
                        _ => Style::default().fg(Color::Gray),
                    };
                    Line::from(Span::styled(format!("! {}", get_fixed(&a.msg)), style))
                })
                .collect();
            f.render_widget(Paragraph::new(lines), alert_rows);
        }
    }

    // ---- footer
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(" quit  "),
            Span::styled("space", Style::default().fg(Color::Cyan)),
            Span::raw(" pause  "),
            Span::styled("+/-", Style::default().fg(Color::Cyan)),
            Span::raw(" rate  "),
            Span::raw(format!(
                "tick {} · gpus {} · procs {}",
                frame.header.tick_id,
                frame.gpus.len(),
                frame.procs.len()
            )),
        ])),
        footer,
    );
}

fn render_gpu_card(
    f: &mut Frame,
    area: ratatui::layout::Rect,
    g: &GpuSampleV1,
    ring: Option<&zionstats_core::shm::layout::HistoryRingV1>,
) {
    let name = get_fixed(&g.name);
    let pstate_str = if g.pstate < 0 {
        "?".to_string()
    } else {
        format!("P{ }", g.pstate).replace(" ", "")
    };
    let title = format!(" GPU {} ─ {} ─ {} ", g.index, name, pstate_str);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(symbols::border::ROUNDED)
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner_area = block.inner(area);
    f.render_widget(block, area);

    let [row1, row2] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(inner_area);
    let [util_c, power_c] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row1);
    let [temp_c, clocks_c] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row2);

    let temp_style = if g.temp_gpu >= 83.0 {
        Color::Red
    } else if g.temp_gpu >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(temp_style))
            .percent(g.util_gpu.clamp(0.0, 100.0) as u16)
            .label(format!("GPU {}%", g.util_gpu)),
        util_c,
    );
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(Color::Magenta))
            .percent(((g.power_w / g.power_limit_w.max(1.0)) * 100.0).clamp(0.0, 100.0) as u16)
            .label(format!("{:.0}/{:.0}W", g.power_w, g.power_limit_w)),
        power_c,
    );
    f.render_widget(
        LineGauge::default()
            .filled_style(Style::default().fg(temp_style))
            .ratio((g.temp_gpu / 100.0).clamp(0.0, 1.0) as f64)
            .label(format!("{:.0}C", g.temp_gpu)),
        temp_c,
    );
    f.render_widget(
        Paragraph::new(format!(
            "SM {}MHz · MEM {:.1}GiB · FAN {:.0}%",
            g.clocks_sm_mhz,
            g.mem_used as f64 / (1 << 30) as f64,
            g.fan_pct
        )),
        clocks_c,
    );

    // sparkline of GPU util from the SHM history ring
    if let Some(ring) = ring {
        let head = ring.head as usize;
        let _ = head;
        let head32 = ring.head;
        let series: Vec<u64> = (0..RING_SHOW)
            .map(|k| {
                let slot = (head32.wrapping_sub((RING_SHOW - k) as u32) as usize)
                    % zionstats_core::shm::layout::RING_LEN;
                let v = ring.data[slot][zionstats_core::shm::layout::RING_UTIL_GPU];
                if v.is_nan() {
                    0
                } else {
                    v.clamp(0.0, 100.0) as u64
                }
            })
            .collect();
        let [_, spark_area] =
            Layout::vertical([Constraint::Percentage(60), Constraint::Min(0)]).areas(row2);
        let _ = spark_area;
        // sparkline occupies a thin band under row2 when the card is tall enough
        if area.height >= 5 {
            let [_, spark] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner_area);
            f.render_widget(
                Sparkline::default()
                    .data(&series)
                    .style(Style::default().fg(Color::Cyan)),
                spark,
            );
        }
    }
}

const RING_SHOW: usize = 96;

fn render_procs(f: &mut Frame, area: ratatui::layout::Rect, procs: &[ProcEntryV1]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(symbols::border::ROUNDED)
        .title(Span::styled(
            format!(" Processes ({}) ", procs.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            "{:<8} {:<4} {:<10} {:>9}  {}",
            "PID", "GPU", "USER", "MEM", "COMMAND"
        ),
        Style::default().fg(Color::DarkGray),
    )));
    for p in procs.iter().take(inner.height as usize).skip(1) {
        lines.push(Line::from(format!(
            "{:<8} {:<4} {:<10} {:>7.1}G  {}",
            p.pid,
            p.gpu_idx,
            get_fixed(&p.user),
            p.mem_used as f64 / (1 << 30) as f64,
            get_fixed(&p.command)
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// compile-time: TUI links against the same layout the daemon writes
const _: () = assert!(zionstats_core::shm::layout::LAYOUT_VERSION == 1);
