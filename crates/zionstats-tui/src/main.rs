//! zionstats-tui — SHM-backed terminal UI (never touches NVML).
use clap::Parser;
use zionstats_tui::app::{run, TuiOpts};

#[derive(Parser, Debug)]
#[command(
    name = "zionstats-tui",
    version,
    about = "Attach to a running zionstatsd and render its shared-memory frames"
)]
struct Cli {
    /// UI refresh seconds (floor 0.25)
    #[arg(short = 'i', long, default_value_t = 0.25)]
    interval: f64,
    #[arg(long)]
    readonly: bool,
    /// hidden: exit after N ms (launch benchmark)
    #[arg(long, hide = true)]
    exit_after_ms: Option<u64>,
    #[arg(long, hide = true)]
    run_once_frame: bool,
    /// hidden: report attach->first-draw latency in the bench line
    #[arg(long, hide = true)]
    print_first_frame_ms: bool,
}

fn main() {
    let cli = Cli::parse();
    let cfg = zionstats_core::config::load(&zionstats_core::config::config_path(None));
    let code = run(TuiOpts {
        theme_name: cfg.theme,
        interval_s: cli.interval,
        readonly: cli.readonly,
        exit_after_ms: cli.exit_after_ms,
        run_once_frame: cli.run_once_frame,
        print_first_frame_ms: cli.print_first_frame_ms,
    });
    std::process::exit(code);
}
