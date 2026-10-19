//! Shared DRM fdinfo scanner (per-process GPU stats for AMD + Intel).
//! Kernel >= 5.14 exposes per-open-client usage in /proc/<pid>/fdinfo/<fd>
//! as drm-* keys; only fds whose symlink target is a /dev/dri node carry
//! them.  Nothing here may panic at scan time (Python drmfd.py parity).

use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrmClient {
    pub pid: u32,
    pub client_id: String,
    pub driver: String,
    pub pdev: String,
    /// numeric drm-* fields of this fd's fdinfo
    pub fields: HashMap<String, u64>,
}

pub fn is_drm_node(target: &str) -> bool {
    target.starts_with("/dev/dri/renderD") || target.starts_with("/dev/dri/card")
}

pub fn parse_fdinfo(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.starts_with("drm-") {
            out.insert(key.to_string(), val.trim().to_string());
        }
    }
    out
}

/// One DrmClient per (pid, drm fd).  Unreadable pids/fds are skipped.
pub fn scan_clients(proc_root: &std::path::Path) -> Vec<DrmClient> {
    let mut clients = Vec::new();
    let Ok(pids) = std::fs::read_dir(proc_root) else {
        return clients;
    };
    for pid in pids.flatten() {
        let Some(name) = pid.file_name().to_str().map(String::from) else {
            continue;
        };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let fd_dir = pid.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let target = match std::fs::read_link(fd.path()) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            if !is_drm_node(&target) {
                continue;
            }
            let text = match std::fs::read_to_string(pid.path().join("fdinfo").join(fd.file_name()))
            {
                Ok(t) => t,
                Err(_) => continue,
            };
            let kv = parse_fdinfo(&text);
            let mut fields = HashMap::new();
            for (k, v) in &kv {
                if k == "drm-client-id" || k == "drm-driver" || k == "drm-pdev" {
                    continue;
                }
                if let Ok(n) = v.parse::<u64>() {
                    fields.insert(k.clone(), n);
                }
            }
            clients.push(DrmClient {
                pid: name.parse().unwrap_or(0),
                client_id: kv
                    .get("drm-client-id")
                    .cloned()
                    .unwrap_or_else(|| format!("{name}:{}", fd.file_name().to_string_lossy())),
                driver: kv.get("drm-driver").cloned().unwrap_or_default(),
                pdev: kv.get("drm-pdev").cloned().unwrap_or_default(),
                fields,
            });
        }
    }
    clients
}

/// drm-cycles-<engine> delta over drm-total-cycles-<engine> delta, summed
/// across matching engines.  None without a two-scan baseline or a zero
/// total window (a stalled ring contributes nothing, not 100%).
pub fn engine_utilization(
    prev: &HashMap<String, u64>,
    cur: &HashMap<String, u64>,
    engine_match: impl Fn(&str) -> bool,
) -> Option<f32> {
    let mut cyc = 0u64;
    let mut tot = 0u64;
    for (k, v) in cur {
        let Some(engine) = k.strip_prefix("drm-cycles-") else {
            continue;
        };
        if !engine_match(engine) {
            continue;
        }
        let tkey = format!("drm-total-cycles-{engine}");
        if let (Some(&pv), Some(&pt)) = (prev.get(k), prev.get(&tkey)) {
            if let Some(&ct) = cur.get(&tkey) {
                cyc += v.saturating_sub(pv);
                tot += ct.saturating_sub(pt);
            }
        }
    }
    if tot == 0 {
        return None;
    }
    Some((cyc.min(tot)) as f32 / tot as f32 * 100.0)
}

pub fn is_render_engine(name: &str) -> bool {
    name.starts_with("rcs") || name.starts_with("ccs") || name.starts_with("gfx")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdinfo_key_filter() {
        let kv = parse_fdinfo("pos:\t480000\nflags:\t0100002\ndrm-driver:\tamdgpu\ndrm-client-id:\t42\ndrm-cycles-gfx:\t1000\n");
        assert_eq!(kv.get("drm-driver").map(String::as_str), Some("amdgpu"));
        assert_eq!(kv.get("drm-cycles-gfx").map(String::as_str), Some("1000"));
        assert!(!kv.contains_key("pos"));
    }

    #[test]
    fn delta_math_first_sighting_and_clamp() {
        use std::collections::HashMap;
        let empty = HashMap::new();
        let cur = [
            ("drm-cycles-gfx".to_string(), 2000u64),
            ("drm-total-cycles-gfx".to_string(), 20000u64),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            engine_utilization(&empty, &cur, |_| true),
            None,
            "no baseline"
        );
        let prev = [
            ("drm-cycles-gfx".to_string(), 1000u64),
            ("drm-total-cycles-gfx".to_string(), 10000u64),
        ]
        .into_iter()
        .collect();
        let u = engine_utilization(&prev, &cur, |_| true).unwrap();
        assert!((u - 10.0).abs() < 1e-6);
        // backwards cycle counter with a moving total clamps to 0
        let backwards: HashMap<String, u64> = [
            ("drm-cycles-gfx".to_string(), 100u64),
            ("drm-total-cycles-gfx".to_string(), 20000u64),
        ]
        .into_iter()
        .collect();
        assert_eq!(engine_utilization(&prev, &backwards, |_| true), Some(0.0));
        // a fully stalled window (nothing moved) is None, not 0
        let stalled: HashMap<String, u64> = [
            ("drm-cycles-gfx".to_string(), 5000u64),
            ("drm-total-cycles-gfx".to_string(), 10000u64),
        ]
        .into_iter()
        .collect();
        assert_eq!(engine_utilization(&prev, &stalled, |_| true), None);
    }

    #[test]
    fn scan_clients_fixture_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/amd/proc");
        let clients = scan_clients(&root);
        let ids: Vec<&str> = clients.iter().map(|c| c.client_id.as_str()).collect();
        assert!(ids.contains(&"42") && ids.contains(&"43"), "got {ids:?}");
        let c42 = clients.iter().find(|c| c.client_id == "42").unwrap();
        assert_eq!(c42.pid, 4242);
        assert_eq!(c42.fields.get("drm-memory-vram"), Some(&4096));
        // the /etc/hosts fd (fd 5) must be skipped
        assert!(!clients.iter().any(|c| c.fields.contains_key("pos")));
    }
}
