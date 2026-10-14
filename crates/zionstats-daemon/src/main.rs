//! zionstatsd — headless GPU monitoring daemon (NVML -> SHM + Prometheus).
use clap::Parser;
use std::path::PathBuf;
use zionstats_daemon::daemon::{run, DaemonOpts};

#[derive(Parser, Debug)]
#[command(name = "zionstatsd", version, about = "zionstats headless GPU daemon")]
struct Cli {
    /// Poll interval seconds (floor 0.25)
    #[arg(short = 'i', long)]
    interval: Option<f64>,
    /// Metrics bind host (default 127.0.0.1)
    #[arg(long)]
    host: Option<String>,
    /// Metrics port (default 9400)
    #[arg(long)]
    port: Option<u16>,
    /// Forbid all actuation (structurally builds no actuators)
    #[arg(long)]
    readonly: bool,
    /// Synthetic two-GPU source (no NVML needed)
    #[arg(long)]
    demo: bool,
    /// Config path (default $ZION_CONFIG or ~/.config/zionstats/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Cooling mode override (off|dry_run|auto|manual)
    #[arg(long)]
    mode: Option<String>,
}

impl From<&Cli> for DaemonOpts {
    fn from(c: &Cli) -> Self {
        DaemonOpts {
            interval_s: c.interval.unwrap_or(0.0),
            host: c.host.clone().unwrap_or_default(),
            port: c.port.unwrap_or(0),
            readonly: c.readonly,
            demo: c.demo,
            config_path: c.config.clone(),
            mode_override: c
                .mode
                .as_deref()
                .and_then(zionstats_core::control::CoolingMode::parse),
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let code = run(DaemonOpts::from(&cli));
    std::process::exit(code);
}
