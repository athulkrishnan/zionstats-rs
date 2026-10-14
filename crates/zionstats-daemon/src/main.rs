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
    /// Print the resolved configuration and exit (never touches NVML)
    #[arg(long)]
    check_config: bool,
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
    if cli.check_config {
        let path = zionstats_core::config::config_path(cli.config.as_deref());
        let (cfg, warnings) = zionstats_core::config::load_verbose(&path);
        let mut overrides = Vec::new();
        if let Some(i) = cli.interval {
            overrides.push(format!("interval = {i}s"));
        }
        if let Some(h) = &cli.host {
            overrides.push(format!("host = {h}"));
        }
        if let Some(p) = cli.port {
            overrides.push(format!("port = {p}"));
        }
        if cli.readonly {
            overrides.push("readonly = true".into());
        }
        if let Some(m) = &cli.mode {
            overrides.push(format!("mode = {m}"));
        }
        std::process::exit(zionstats_daemon::check_config::print_resolved(
            &cfg, &warnings, &overrides,
        ));
    }
    let code = run(DaemonOpts::from(&cli));
    std::process::exit(code);
}
