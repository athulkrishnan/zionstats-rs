//! Embedded Prometheus/OpenMetrics + JSON snapshot endpoint (hyper 1).
//! Serves the last-published frame; never touches NVML from the request
//! path. Default bind 127.0.0.1:9400.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;

use zionstats_core::metrics::{
    render_metrics, MetricsInput, CONTENT_TYPE_OPENMETRICS, CONTENT_TYPE_PLAIN,
};
use zionstats_core::shm::layout::ProcEntryV1;

/// Snapshot of the last tick, shared by the poll loop with the HTTP task.
pub struct LiveFrame {
    pub gpus: Vec<zionstats_core::shm::layout::GpuSampleV1>,
    pub procs: Vec<ProcEntryV1>,
    pub control: Option<zionstats_core::shm::layout::ControlStateV1>,
    pub hostname: String,
    pub started_ms: u64,
}

pub type SharedFrame = std::sync::Mutex<LiveFrame>;

pub async fn serve(
    host: &str,
    port: u16,
    frame: Arc<SharedFrame>,
    version: &'static str,
    expose_processes: bool,
) -> Result<(), String> {
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("metrics bind {addr}: {e}"))?;
    log::info!("metrics: http://{addr}/metrics");
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("accept: {e}"))?;
        let io = hyper_util::rt::TokioIo::new(stream);
        let frame = frame.clone();
        tokio::task::spawn(async move {
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let frame = frame.clone();
                async move {
                    let expose = expose_processes;
                    Ok::<_, std::convert::Infallible>(route(req, &frame, version, expose))
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, service)
                .await;
        });
    }
}

fn route(
    req: Request<hyper::body::Incoming>,
    frame: &Arc<SharedFrame>,
    version: &str,
    expose_processes: bool,
) -> Response<Full<Bytes>> {
    let path = req.uri().path();
    let openmetrics = req
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("openmetrics"))
        .unwrap_or(false);
    match path {
        "/metrics" => {
            let lf = frame.lock().unwrap();
            let input = MetricsInput {
                gpus: &lf.gpus,
                procs: &lf.procs,
                control: lf.control.as_ref(),
                hostname: &lf.hostname,
                version,
                uptime_s: (zionstats_core::shm::layout::now_ms().saturating_sub(lf.started_ms))
                    / 1000,
                last_publish_age_ms: 0,
                expose_processes,
            };
            let body = render_metrics(&input, openmetrics);
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    "content-type",
                    if openmetrics {
                        CONTENT_TYPE_OPENMETRICS
                    } else {
                        CONTENT_TYPE_PLAIN
                    },
                )
                .body(Full::new(Bytes::from(body)))
                .unwrap_or_else(|_| empty(500))
        }
        "/snapshot" => {
            let lf = frame.lock().unwrap();
            let snap = serde_json::json!({
                "hostname": lf.hostname,
                "ts_ms": zionstats_core::shm::layout::now_ms(),
                "gpus": lf.gpus.iter().map(gpu_json).collect::<Vec<_>>(),
                "processes": lf.procs.iter().map(proc_json).collect::<Vec<_>>(),
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(snap.to_string())))
                .unwrap_or_else(|_| empty(500))
        }
        "/healthz" => {
            let lf = frame.lock().unwrap();
            let tick_age = zionstats_core::shm::layout::now_ms()
                .saturating_sub(lf.gpus.iter().map(|g| g.ts_ms).max().unwrap_or(0));
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain")
                .body(Full::new(Bytes::from(format!(
                    "ok tick_age_ms={tick_age}\n"
                ))))
                .unwrap_or_else(|_| empty(500))
        }
        _ => empty(404),
    }
}

fn empty(code: u16) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

fn fixed(src: &[u8]) -> String {
    let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
    String::from_utf8_lossy(&src[..end]).into_owned()
}

fn gpu_json(g: &zionstats_core::shm::layout::GpuSampleV1) -> serde_json::Value {
    serde_json::json!({
        "index": g.index,
        "name": fixed(&g.name),
        "uuid": fixed(&g.uuid),
        "util_gpu": g.util_gpu,
        "util_mem": g.util_mem,
        "mem_used": g.mem_used,
        "mem_total": g.mem_total,
        "power_w": g.power_w,
        "power_limit_w": g.power_limit_w,
        "temp_gpu": g.temp_gpu,
        "fan_pct": g.fan_pct,
        "clocks_sm_mhz": g.clocks_sm_mhz,
        "throttle_bits": g.throttle_bits,
        "verdict": g.verdict,
    })
}

fn proc_json(p: &ProcEntryV1) -> serde_json::Value {
    serde_json::json!({
        "pid": p.pid,
        "gpu": p.gpu_idx,
        "mem_used": p.mem_used,
        "command": fixed(&p.command),
        "user": fixed(&p.user),
    })
}

/// Interval helper re-export to keep the dep graph obvious
pub type KeepAlive = Duration;
