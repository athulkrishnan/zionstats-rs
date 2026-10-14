//! Prometheus / OpenMetrics rendering — pure, golden-tested. Metric
//! families match the Python exporter (`zionstats_gpu_*` /
//! `zionstats_process_*`) so the shipped Grafana dashboard keeps working;
//! NaN/None fields omit the series entirely (never emitted).

use crate::shm::layout::{ControlStateV1, GpuSampleV1, ProcEntryV1};

pub const CONTENT_TYPE_PLAIN: &str = "text/plain; version=0.0.4; charset=utf-8";
pub const CONTENT_TYPE_OPENMETRICS: &str =
    "application/openmetrics-text; version=1.0.0; charset=utf-8";

pub struct MetricsInput<'a> {
    pub gpus: &'a [GpuSampleV1],
    pub procs: &'a [ProcEntryV1],
    pub control: Option<&'a ControlStateV1>,
    pub hostname: &'a str,
    pub version: &'a str,
    pub uptime_s: u64,
    pub last_publish_age_ms: u64,
    pub expose_processes: bool,
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn num(v: f32) -> Option<String> {
    if v.is_nan() || v.is_infinite() {
        None
    } else {
        Some(format!("{v}"))
    }
}

fn fixed_str(src: &[u8]) -> String {
    let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
    String::from_utf8_lossy(&src[..end]).into_owned()
}

/// Render the /metrics payload. `openmetrics` selects the OpenMetrics
/// content family (adds `# EOF`).
pub fn render_metrics(m: &MetricsInput, openmetrics: bool) -> String {
    let mut out: Vec<(String, String, &'static str, Vec<String>)> = Vec::new();
    let mut idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    macro_rules! series {
        ($name:expr, $help:expr, $mtype:expr, $labels:expr, $value:expr) => {
            let maybe: Option<f32> = $value;
            if let Some(v) = maybe.and_then(num) {
                let row = format!("zionstats_{}{{{}}} {}", $name, $labels, v);
                match idx.get($name) {
                    Some(&i) => out[i].3.push(row),
                    None => {
                        idx.insert($name.to_string(), out.len());
                        out.push(($name.to_string(), $help.to_string(), $mtype, vec![row]));
                    }
                }
            }
        };
    }

    for g in m.gpus {
        let name = fixed_str(&g.name);
        let uuid = fixed_str(&g.uuid);
        let labels = format!(
            "gpu=\"{}\",name=\"{}\",uuid=\"{}\",device=\"{}\",modelName=\"{}\",Hostname=\"{}\"",
            g.index,
            esc(&name),
            esc(&uuid),
            g.index,
            esc(&name),
            esc(m.hostname)
        );
        series!(
            "gpu_utilization_percent",
            "GPU utilization (%)",
            "gauge",
            labels,
            Some(g.util_gpu)
        );
        series!(
            "gpu_memory_utilization_percent",
            "Memory-bandwidth utilization (%)",
            "gauge",
            labels,
            Some(g.util_mem)
        );
        series!(
            "gpu_memory_used_bytes",
            "VRAM used (bytes)",
            "gauge",
            labels,
            Some(g.mem_used as f32)
        );
        series!(
            "gpu_memory_total_bytes",
            "VRAM total (bytes)",
            "gauge",
            labels,
            Some(g.mem_total as f32)
        );
        series!(
            "gpu_power_watts",
            "Instantaneous power draw (W)",
            "gauge",
            labels,
            Some(g.power_w)
        );
        series!(
            "gpu_power_limit_watts",
            "Enforced power limit (W)",
            "gauge",
            labels,
            Some(g.power_limit_w)
        );
        series!(
            "gpu_temperature_celsius",
            "GPU temperature (C)",
            "gauge",
            format!("{labels},sensor=\"gpu\""),
            Some(g.temp_gpu)
        );
        series!(
            "gpu_temperature_celsius",
            "Memory temperature (C)",
            "gauge",
            format!("{labels},sensor=\"memory\""),
            Some(g.temp_mem)
        );
        series!(
            "gpu_clock_sm_mhz",
            "SM clock (MHz)",
            "gauge",
            labels,
            Some(g.clocks_sm_mhz as f32)
        );
        series!(
            "gpu_clock_mem_mhz",
            "Memory clock (MHz)",
            "gauge",
            labels,
            Some(g.clocks_mem_mhz as f32)
        );
        series!(
            "gpu_fan_percent",
            "Fan speed (%)",
            "gauge",
            labels,
            Some(g.fan_pct)
        );
        series!(
            "gpu_fan_pwm_set_percent",
            "Last commanded PWM (absent when driver owns fans)",
            "gauge",
            labels,
            Some(g.fan_pwm_set)
        );
        series!(
            "gpu_pcie_link_gen",
            "PCIe link generation",
            "gauge",
            labels,
            Some(g.pcie_gen as f32)
        );
        series!(
            "gpu_pcie_link_width",
            "PCIe link width (lanes)",
            "gauge",
            labels,
            Some(g.pcie_width as f32)
        );
        series!(
            "gpu_pcie_rx_bytes_per_second",
            "PCIe read throughput (bytes/s)",
            "gauge",
            labels,
            Some(g.pcie_rx_bps as f32)
        );
        series!(
            "gpu_pcie_tx_bytes_per_second",
            "PCIe write throughput (bytes/s)",
            "gauge",
            labels,
            Some(g.pcie_tx_bps as f32)
        );
        series!(
            "gpu_throttle_bitmask",
            "NVML clocksThrottleReasons bitmask",
            "gauge",
            labels,
            Some(g.throttle_bits as f32)
        );
        series!(
            "gpu_energy_consumption_joules_total",
            "Cumulative energy since driver load (J; resets on driver reload)",
            "counter",
            labels,
            Some(g.energy_mj as f32 / 1000.0)
        );
    }

    if m.expose_processes {
        for p in m.procs {
            let command = fixed_str(&p.command);
            let command = command.chars().take(80).collect::<String>();
            let user = fixed_str(&p.user);
            let mut labels = format!(
                "gpu=\"{}\",pid=\"{}\",user=\"{}\",type=\"{}\",command=\"{}\"",
                p.gpu_idx,
                p.pid,
                esc(&user),
                kind_str(p.kind),
                esc(&command)
            );
            if p.flags & crate::shm::layout::PROC_FLAG_HAS_CONTAINER != 0 {
                labels.push_str(&format!(",container_id=\"{}\"", fixed_str(&p.container_id)));
            }
            if p.flags & crate::shm::layout::PROC_FLAG_HAS_POD != 0 {
                labels.push_str(&format!(",pod=\"{}\"", esc(&fixed_str(&p.pod_name))));
            }
            series!(
                "process_memory_bytes",
                "Per-process GPU memory (bytes)",
                "gauge",
                labels.clone(),
                Some(p.mem_used as f32)
            );
            series!(
                "process_utilization_percent",
                "Per-process GPU utilization (%)",
                "gauge",
                labels,
                Some(p.sm_pct)
            );
            let _ = labels;
        }
    }

    if let Some(c) = m.control {
        let row = format!(
            "zionstats_control_state{{mode=\"{}\",state=\"{}\"}} {}",
            mode_str(c.mode),
            if c.state_on != 0 { "on" } else { "off" },
            if c.state_on != 0 { 1 } else { 0 }
        );
        match idx.get("control_state") {
            Some(&i) => out[i].3.push(row),
            None => {
                idx.insert("control_state".to_string(), out.len());
                out.push((
                    "control_state".to_string(),
                    "Cooling controller state (1 = plug/PWM engaged)".to_string(),
                    "gauge",
                    vec![row],
                ));
            }
        }
        let row = format!("zionstats_control_duty_seconds {}", c.duty_seconds);
        match idx.get("control_duty_seconds") {
            Some(&i) => out[i].3.push(row),
            None => {
                idx.insert("control_duty_seconds".to_string(), out.len());
                out.push((
                    "control_duty_seconds".to_string(),
                    "Continuous actuation seconds in the current duty window".to_string(),
                    "gauge",
                    vec![row],
                ));
            }
        }
        if c.pwm_engaged != 0 {
            let row = format!("zionstats_control_pwm_percent {}", c.pwm_current);
            match idx.get("control_pwm_percent") {
                Some(&i) => out[i].3.push(row),
                None => {
                    idx.insert("control_pwm_percent".to_string(), out.len());
                    out.push((
                        "control_pwm_percent".to_string(),
                        "Current commanded PWM percent (mean across GPUs)".to_string(),
                        "gauge",
                        vec![row],
                    ));
                }
            }
        }
    }

    out.push((
        "daemon_uptime_seconds".to_string(),
        "Daemon uptime (s)".to_string(),
        "gauge",
        vec![format!("zionstats_daemon_uptime_seconds {}", m.uptime_s)],
    ));
    out.push((
        "daemon_last_publish_timestamp_seconds".to_string(),
        "Unix time of the last SHM publish".to_string(),
        "gauge",
        vec![format!(
            "zionstats_daemon_last_publish_timestamp_seconds {}",
            (crate::shm::layout::now_ms().saturating_sub(m.last_publish_age_ms)) as f64 / 1000.0
        )],
    ));
    out.push((
        "build_info".to_string(),
        "Build metadata".to_string(),
        "gauge",
        vec![format!(
            "zionstats_build_info{{version=\"{}\"}} 1",
            esc(m.version)
        )],
    ));

    let mut text = String::with_capacity(8192);
    for (name, help, mtype, rows) in &out {
        text.push_str(&format!("# HELP zionstats_{name} {help}\n"));
        text.push_str(&format!("# TYPE zionstats_{name} {mtype}\n"));
        for r in rows {
            text.push_str(r);
            text.push('\n');
        }
    }
    if openmetrics {
        text.push_str("# EOF\n");
    }
    text
}

fn kind_str(k: u8) -> &'static str {
    match k {
        0 => "C",
        1 => "G",
        2 => "C+G",
        _ => "C",
    }
}

fn mode_str(m: u8) -> &'static str {
    match m {
        0 => "off",
        1 => "dry_run",
        2 => "auto",
        3 => "manual",
        _ => "off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::layout::*;

    fn sample(name: &str, util: f32) -> GpuSampleV1 {
        let mut g = GpuSampleV1 {
            ts_ms: now_ms(),
            uuid: [0; 48],
            name: [0; 64],
            mem_used: 2 << 30,
            mem_total: 16 << 30,
            energy_mj: 3_600_000,
            util_gpu: util,
            util_mem: 40.0,
            util_enc: f32::NAN,
            util_dec: f32::NAN,
            power_w: 120.0,
            power_limit_w: 300.0,
            temp_gpu: 55.0,
            temp_mem: f32::NAN,
            fan_pct: 40.0,
            fan_pwm_set: f32::NAN,
            clocks_sm_mhz: 1800,
            clocks_mem_mhz: 9000,
            pcie_rx_bps: 1_000_000,
            pcie_tx_bps: 500_000,
            throttle_bits: 4,
            index: 0,
            pcie_gen: 4,
            pcie_width: 16,
            pstate: 0,
            verdict: 0,
            flags: 0,
        };
        put_fixed(&mut g.name, name);
        put_fixed(&mut g.uuid, "GPU-test");
        g
    }

    #[test]
    fn golden_families_and_escaping() {
        let g = sample("Weird \"GPU\" \\name", 42.0);
        let m = MetricsInput {
            gpus: &[g],
            procs: &[],
            control: None,
            hostname: "box1",
            version: "0.5.0",
            uptime_s: 10,
            last_publish_age_ms: 0,
            expose_processes: false,
        };
        let text = render_metrics(&m, false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "# HELP zionstats_gpu_utilization_percent GPU utilization (%)"
        );
        assert_eq!(lines[1], "# TYPE zionstats_gpu_utilization_percent gauge");
        assert!(text.contains(
            "zionstats_gpu_utilization_percent{gpu=\"0\",name=\"Weird \\\"GPU\\\" \\\\name\",uuid=\"GPU-test\",device=\"0\",modelName=\"Weird \\\"GPU\\\" \\\\name\",Hostname=\"box1\"} 42"
        ));
        assert!(
            text.contains("zionstats_gpu_pcie_rx_bytes_per_second") && text.contains("1000000")
        );
        assert!(
            text.contains("zionstats_gpu_energy_consumption_joules_total") && text.contains("3600")
        );
        assert!(text.contains("zionstats_build_info{version=\"0.5.0\"} 1"));
        assert!(
            !text.contains("zionstats_gpu_fan_pwm_set_percent"),
            "NaN pwm omitted"
        );
        assert!(!text.to_lowercase().contains("nan"));
        // HELP appears exactly once per family
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("# HELP zionstats_gpu_utilization_percent"))
                .count(),
            1
        );
    }

    #[test]
    fn openmetrics_eof() {
        let g = sample("x", 1.0);
        let m = MetricsInput {
            gpus: &[g],
            procs: &[],
            control: None,
            hostname: "h",
            version: "0.5.0",
            uptime_s: 1,
            last_publish_age_ms: 0,
            expose_processes: false,
        };
        assert!(render_metrics(&m, true).ends_with("# EOF\n"));
        assert!(!render_metrics(&m, false).ends_with("# EOF\n"));
    }

    #[test]
    fn empty_input_still_serves_build_info() {
        let m = MetricsInput {
            gpus: &[],
            procs: &[],
            control: None,
            hostname: "h",
            version: "9.9",
            uptime_s: 0,
            last_publish_age_ms: 0,
            expose_processes: false,
        };
        let text = render_metrics(&m, false);
        assert!(text.contains("zionstats_build_info{version=\"9.9\"} 1"));
        assert!(!text.contains("gpu_utilization_percent\n#"));
    }
}
