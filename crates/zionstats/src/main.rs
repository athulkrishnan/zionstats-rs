//! Unified CLI: `zionstats` runs the TUI by default (P6), `--daemon` the daemon.
use clap::Parser;
use std::path::PathBuf;

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
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
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
        };
        zionstats_daemon::daemon::run(opts)
    } else {
        // P6: attach the shared-memory TUI. Until then, point at the daemon.
        eprintln!("zionstats-tui lands in v0.5 P6 -- meanwhile: `zionstatsd --demo` publishes SHM");
        0
    };
    std::process::exit(exit);
}
