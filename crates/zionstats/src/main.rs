//! Unified CLI: `zionstats` runs the TUI by default (P6), `--daemon` the daemon.
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use std::path::PathBuf;

mod demo_child;

#[derive(Parser, Debug)]
#[command(
    name = "zionstats",
    version,
    about = "btop-grade NVIDIA GPU observatory (Rust core: SHM-backed TUI + headless daemon)"
)]
struct Cli {
    /// Run the headless daemon instead of the TUI
    #[arg(long)]
    daemon: bool,
    /// Interval seconds: TUI refresh / daemon poll (floor 0.25)
    #[arg(short = 'i', long)]
    interval: Option<f64>,
    /// Daemon metrics bind host
    #[arg(long)]
    host: Option<String>,
    /// Daemon metrics port
    #[arg(long)]
    port: Option<u16>,
    /// Forbid all actuation
    #[arg(long)]
    readonly: bool,
    /// Synthetic two-GPU source (no NVML needed)
    #[arg(long)]
    demo: bool,
    /// Config path (default $ZION_CONFIG or ~/.config/zionstats/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Cooling mode override (off|dry_run|auto|manual) [daemon only]
    #[arg(long)]
    mode: Option<String>,
    /// Replay a recorded trace (TUI over an embedded replay daemon)
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Emit shell completions for the given shell and exit
    #[arg(long, value_enum)]
    completions: Option<Shell>,
    /// hidden: TUI reports attach->first-draw latency (bench mode)
    #[arg(long, hide = true)]
    print_first_frame_ms: bool,
    /// hidden: TUI exits after N ms (launch benchmark)
    #[arg(long, hide = true)]
    exit_after_ms: Option<u64>,
    #[arg(long, hide = true)]
    run_once_frame: bool,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();

    // `zionstats completions <shell>`: emit and exit
    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        return;
    }

    let exit = if cli.daemon {
        let opts = zionstats_daemon::daemon::DaemonOpts {
            interval_s: cli.interval.unwrap_or(0.0),
            host: cli.host.clone().unwrap_or_default(),
            port: cli.port.unwrap_or(0),
            readonly: cli.readonly,
            demo: cli.demo,
            config_path: cli.config.clone(),
            mode_override: cli
                .mode
                .as_deref()
                .and_then(zionstats_core::control::CoolingMode::parse),
            record: None,
            replay: cli.replay.clone(),
        };
        zionstats_daemon::daemon::run(opts)
    } else {
        run_tui_mode(&cli)
    };
    std::process::exit(exit);
}

fn run_tui_mode(cli: &Cli) -> i32 {
    use zionstats_core::shm::ShmReader;
    let cfg =
        zionstats_core::config::load(&zionstats_core::config::config_path(cli.config.as_deref()));
    let interval = cli.interval.unwrap_or(cfg.tui_interval_s);
    let theme_name = {
        let cfg = zionstats_core::config::load(&zionstats_core::config::config_path(
            cli.config.as_deref(),
        ));
        cfg.theme
    };
    let tui_opts = |exit_after_ms: Option<u64>, print_ff: bool| zionstats_tui::app::TuiOpts {
        theme_name: theme_name.clone(),
        interval_s: interval,
        readonly: cli.readonly,
        exit_after_ms,
        run_once_frame: cli.run_once_frame,
        print_first_frame_ms: print_ff,
    };

    // live daemon already publishing? attach directly.
    if ShmReader::attach(unsafe { libc::getuid() }).is_ok() {
        return zionstats_tui::app::run(tui_opts(cli.exit_after_ms, cli.print_first_frame_ms));
    }
    // --replay: spawn an embedded replay daemon (same orphan-proofing as
    // the demo child), then attach
    if let Some(replay_path) = cli.replay.clone() {
        match demo_child::spawn_with_args(
            cli.config.as_deref(),
            cli.readonly,
            &["--replay", replay_path.to_string_lossy().as_ref()],
        ) {
            Ok(mut child) => {
                match demo_child::wait_for_first_frame(&mut child, 2000) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!("zionstats: a zionstats writer is already active; attaching to live frames");
                    }
                    Err(e) => {
                        child.shutdown();
                        eprintln!("zionstats: replay daemon error: {e}");
                        return 2;
                    }
                }
                return zionstats_tui::app::run(tui_opts(
                    cli.exit_after_ms,
                    cli.print_first_frame_ms,
                ));
            }
            Err(e) => {
                eprintln!("zionstats: --replay daemon failed to start: {e}");
                return 2;
            }
        }
    }
    // --demo: spawn an embedded demo daemon, then attach
    if cli.demo {
        match demo_child::spawn(cli.config.as_deref(), cli.readonly) {
            Ok(mut child) => {
                match demo_child::wait_for_first_frame(&mut child, 2000) {
                    Ok(true) => {}
                    Ok(false) => {
                        // flock held by a real writer (child exited): attach to it
                        eprintln!("zionstats: a zionstats writer is already active; attaching to live frames");
                    }
                    Err(e) => {
                        child.shutdown();
                        eprintln!("zionstats: demo daemon error: {e}");
                        return 2;
                    }
                }
                return zionstats_tui::app::run(tui_opts(
                    cli.exit_after_ms,
                    cli.print_first_frame_ms,
                ));
            }
            Err(e) => {
                eprintln!("zionstats: --demo daemon failed to start: {e}");
                return 2;
            }
        }
    }
    eprintln!(
        "zionstats: no daemon publishing to shared memory -- start one with:\n  zionstats --demo        (synthetic two-GPU rig, no GPU needed)\n  zionstats --daemon      (live NVML daemon, then re-run zionstats)"
    );
    2
}
