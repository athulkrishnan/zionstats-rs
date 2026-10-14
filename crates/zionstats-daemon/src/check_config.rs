//! `--check-config`: print the resolved configuration and any per-key
//! fallback warnings. Pure introspection -- NEVER builds the control stack
//! (which would init NVML and write fan PWM).

use zionstats_core::config::{self, Config};
use zionstats_core::control::{CoolingMode, Trigger};

pub fn print_resolved(cfg: &Config, warnings: &[String], overrides: &[String]) -> i32 {
    println!("resolved configuration:");
    println!("  interval (tui)      = {:.2}s", cfg.tui_interval_s);
    println!("  readonly            = {}", cfg.readonly);
    let d = &cfg.daemon;
    println!("  [daemon]");
    println!("    interval          = {:.2}s", d.interval_s);
    println!("    host:port         = {}:{}", d.host, d.port);
    println!("    readonly          = {}", d.readonly);
    println!("    ambient_temp      = {:.1}C", d.ambient_temp_c);
    println!(
        "    fan_control       = {} (opt-in GPU fan PWM)",
        d.fan_control
    );
    println!("    fan_pwm_step      = {}%", d.fan_pwm_step);
    let curve: Vec<String> = d
        .fan_curve
        .iter()
        .map(|(t, p)| format!("({t},{p})"))
        .collect();
    println!("    fan_curve         = {}", curve.join(" "));
    println!("    shutdown_plugs    = {}", d.shutdown_plug_policy);
    if let Some(g) = &d.shm_group {
        println!("    shm_group         = {g}");
    }
    let c = &cfg.cooling;
    println!("  [cooling]");
    println!("    mode              = {}", mode_str(c.mode));
    println!("    trigger           = {}", trigger_str(c.trigger));
    println!(
        "    trigger/resume    = {:.0}C / {:.0}C (min delta {:.0}C over ambient)",
        c.trigger_temp, c.resume_temp, c.min_delta_c
    );
    println!(
        "    min on/off        = {:.0}s / {:.0}s, duty cap {:.0}min",
        c.min_on_s, c.min_off_s, c.max_duty_min
    );
    if c.trigger == Trigger::Usage {
        println!(
            "    usage trigger     = ON at {:.0}% util, OFF after {:.0}s idle",
            c.usage_util, c.idle_off_s
        );
    }
    println!("  [[cooling.plugs]] ({} entries)", cfg.plugs.len());
    for p in &cfg.plugs {
        match p.backend.as_str() {
            "shelly" | "tasmota" => {
                println!(
                    "    {}: {} @ {} (gen {}, channel {})",
                    p.backend, p.name, p.host, p.gen, p.channel
                );
            }
            "tuya" | "kasa" | "ha" => {
                println!(
                    "    {}: {} @ {} -- needs the Python daemon (zionstats --log); skipped here",
                    p.backend, p.name, p.host
                );
            }
            other => println!("    {other}: UNKNOWN backend -- will be skipped with a warning"),
        }
    }
    println!("  [export]");
    println!(
        "    expose_processes  = {} (metrics socket is [daemon] host:port)",
        cfg.expose_processes
    );
    if !overrides.is_empty() {
        println!("  CLI overrides:");
        for o in overrides {
            println!("    {o}");
        }
    }
    if warnings.is_empty() {
        println!("config warnings: none");
    } else {
        println!("config warnings:");
        for w in warnings {
            println!("  - {w}");
        }
    }
    if cfg.readonly || cfg.daemon.readonly {
        println!("actuation: DISABLED (readonly)");
    } else if cfg.plugs.is_empty() && !cfg.daemon.fan_control {
        println!("actuation: none configured (no plugs, fan_control off)");
    } else if cfg.cooling.mode == CoolingMode::Auto {
        println!("actuation: ARMED (mode auto) -- verify with --plug-test equivalents first");
    } else {
        println!(
            "actuation: configured but mode is {}",
            mode_str(cfg.cooling.mode)
        );
    }
    let fatal = warnings.iter().any(|w| w.contains("not valid TOML"));
    if fatal {
        1
    } else {
        0
    }
}

fn mode_str(m: CoolingMode) -> &'static str {
    m.as_str()
}

fn trigger_str(t: Trigger) -> &'static str {
    match t {
        Trigger::Temp => "temp",
        Trigger::Usage => "usage",
    }
}
