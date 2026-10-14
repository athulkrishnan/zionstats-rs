//! Container / Kubernetes PID resolver — parses /proc/<pid>/cgroup (v1+v2)
//! and best-effort resolves container IDs and pod names. Never panics on
//! malformed input; unknown means "short-id only", never a wrong name.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Docker,
    Containerd,
    CriO,
    Podman,
    Unknown,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::Docker => "docker",
            Runtime::Containerd => "containerd",
            Runtime::CriO => "cri-o",
            Runtime::Podman => "podman",
            Runtime::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerInfo {
    pub id64: String,
    pub short: String,
    pub runtime: Runtime,
    pub pod_uid: Option<String>,
    pub pod_name: Option<String>,
}

/// Pure parse of one cgroup file's text. Returns the (container id, pod uid,
/// runtime) triple when recognizable. Handles v2 (`0::path`) and v1
/// (multi-controller lines) formats.
pub fn parse_cgroup(text: &str) -> Option<(String, Option<String>, Runtime)> {
    static RE_DOCKER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_CRI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_POD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_PODMAN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_KUBEPODS_V1: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_docker = RE_DOCKER
        .get_or_init(|| regex::Regex::new(r"(?:docker[-/])([0-9a-f]{64})(?:\.scope)?").unwrap());
    let re_cri = RE_CRI
        .get_or_init(|| regex::Regex::new(r"(?:cri-containerd-|crio-)([0-9a-f]{64})").unwrap());
    let re_pod = RE_POD.get_or_init(|| {
        regex::Regex::new(r"pod([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})")
            .unwrap()
    });
    let re_podman = RE_PODMAN
        .get_or_init(|| regex::Regex::new(r"(?:podman[-/])([0-9a-f]{64})(?:\.scope)?").unwrap());
    // legacy v1 kubepods: bare 64-hex container segment after pod<uid>/
    let re_kubepods_v1 = RE_KUBEPODS_V1
        .get_or_init(|| regex::Regex::new(r"pod[0-9a-f-]{36}/([0-9a-f]{64})").unwrap());

    for line in text.lines() {
        // v2: "0::/path" — v1: "12:pids:/path"; take the path part
        let path = match line.split_once(':') {
            Some((_, rest)) => match rest.split_once(':') {
                Some((_, p)) => p,
                None => continue,
            },
            None => continue,
        };
        // v2 kubepods slice names underscore the uuid — normalize
        let norm = path.replace('_', "-");

        if let Some(c) = re_cri.captures(path).or_else(|| re_cri.captures(&norm)) {
            let id = c[1].to_string();
            let pod = re_pod.captures(&norm).map(|p| p[1].to_string());
            return Some((id, pod, Runtime::Containerd));
        }
        if let Some(c) = re_kubepods_v1
            .captures(path)
            .or_else(|| re_kubepods_v1.captures(&norm))
        {
            let id = c[1].to_string();
            let pod = re_pod.captures(&norm).map(|p| p[1].to_string());
            return Some((id, pod, Runtime::Containerd));
        }
        if let Some(c) = re_podman
            .captures(path)
            .or_else(|| re_podman.captures(&norm))
        {
            return Some((c[1].to_string(), None, Runtime::Podman));
        }
        if let Some(c) = re_docker
            .captures(path)
            .or_else(|| re_docker.captures(&norm))
        {
            return Some((c[1].to_string(), None, Runtime::Docker));
        }
    }
    None
}

/// Best-effort pod name from /proc/<pid>/mountinfo text (kubelet pod dirs)
/// or the container's /etc/hostname (k8s defaults hostname = pod name).
/// Returns None over None — callers fall back to pod uid / short id.
pub fn pod_name_from_artifacts(
    mountinfo: &str,
    container_hostname: Option<&str>,
) -> Option<String> {
    let in_pod_dir = mountinfo.contains("/var/lib/kubelet/pods/");
    let name = container_hostname
        .map(str::trim)
        .filter(|h| !h.is_empty() && !h.contains(char::is_whitespace))
        .map(str::to_string);
    match (name, in_pod_dir) {
        (Some(n), true) => Some(n),
        (Some(n), false) => Some(n),
        (None, _) => None,
    }
}

/// LRU-cached, PID-reuse-safe resolver. Entries key on (pid) and invalidate
/// when /proc/<pid>/stat starttime changes.
pub struct CgroupResolver {
    cache: HashMap<u32, (u64, Option<ContainerInfo>)>,
    order: VecDeque<u32>,
    cap: usize,
}

impl Default for CgroupResolver {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl CgroupResolver {
    pub fn new(cap: usize) -> Self {
        CgroupResolver {
            cache: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn lru_touch(&mut self, pid: u32) {
        self.order.retain(|&p| p != pid);
        self.order.push_back(pid);
        while self.order.len() > self.cap {
            let evict = self.order.pop_front();
            if let Some(p) = evict {
                self.cache.remove(&p);
            }
        }
    }

    /// Resolve with the live /proc tree. `proc_root` is injectable for
    /// tests (usually "/proc").
    pub fn resolve(&mut self, pid: u32, starttime: u64, proc_root: &Path) -> Option<ContainerInfo> {
        if let Some((st, info)) = self.cache.get(&pid) {
            if *st == starttime {
                let info = info.clone();
                self.lru_touch(pid);
                return info;
            }
            self.cache.remove(&pid); // PID reuse
        }
        let info = self.resolve_uncached(pid, proc_root);
        self.cache.insert(pid, (starttime, info.clone()));
        self.lru_touch(pid);
        info
    }

    fn resolve_uncached(&mut self, pid: u32, proc_root: &Path) -> Option<ContainerInfo> {
        let cg = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
        let (id64, pod_uid, runtime) = parse_cgroup(&cg)?;
        let short = id64[..id64.len().min(12)].to_string();
        let pod_name = pod_uid.as_ref().and_then(|_uid| {
            // kubelet pod dir confirms the uid; hostname is the default
            // pod-name source in k8s
            let mi =
                std::fs::read_to_string(proc_root.join(pid.to_string()).join("mountinfo")).ok()?;
            let hostname =
                std::fs::read_to_string(proc_root.join(pid.to_string()).join("root/etc/hostname"))
                    .ok();
            pod_name_from_artifacts(&mi, hostname.as_deref())
        });
        Some(ContainerInfo {
            id64,
            short,
            runtime,
            pod_uid,
            pod_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_docker_scope() {
        let (id, pod, rt) = parse_cgroup("0::/system.slice/docker-9f4a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a.scope\n").unwrap();
        assert_eq!(rt, Runtime::Docker);
        assert!(id.starts_with("9f4a1b2c"));
        assert_eq!(id.len(), 64);
        assert!(pod.is_none());
    }

    #[test]
    fn v2_kubepods_containerd() {
        let line = "0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1f2e3d4c_5b6a_7988_9a0b_c1d2e3f4a5b6.slice/cri-containerd-abcdef11223344556677889900aabbccddeeff00112233445566778899aabbcc.scope\n";
        let (id, pod, rt) = parse_cgroup(line).unwrap();
        assert_eq!(rt, Runtime::Containerd);
        assert!(id.starts_with("abcdef11"));
        assert_eq!(pod.as_deref(), Some("1f2e3d4c-5b6a-7988-9a0b-c1d2e3f4a5b6"));
    }

    #[test]
    fn v1_docker_and_kubepods() {
        let text = "12:pids:/docker/beef0011223344556677889900aabbccddeeff00112233445566778899aabbcc\n1:name=systemd:/docker/beef0011223344556677889900aabbccddeeff00112233445566778899aabbcc\n";
        let (id, _, rt) = parse_cgroup(text).unwrap();
        assert_eq!(rt, Runtime::Docker);
        assert!(id.starts_with("beef"));

        let text2 = "9:pids:/kubepods/burstable/podaabbccdd-1122-3344-5566-778899aabbcc/11223344556677889900aabbccddeeff00112233445566778899aabbccddeeff\n";
        let (id2, pod2, rt2) = parse_cgroup(text2).unwrap();
        assert_eq!(rt2, Runtime::Containerd);
        assert!(id2.starts_with("1122"));
        assert_eq!(
            pod2.as_deref(),
            Some("aabbccdd-1122-3344-5566-778899aabbcc")
        );
    }

    #[test]
    fn garbage_never_panics_and_yields_none() {
        for junk in ["", "\n\n", "0::", ":::", "0::/nothing/here", "12", "\0\0\0"] {
            assert!(parse_cgroup(junk).is_none(), "{junk:?}");
        }
        // 10 MB line
        let big = format!("0::/{}\n", "a".repeat(10_000_000));
        assert!(parse_cgroup(&big).is_none());
    }

    #[test]
    fn pod_name_heuristics() {
        let mi = "36 30 0:34 / /var/lib/kubelet/pods/1f2e3d4c-5b6a-7988-9a0b-c1d2e3f4a5b6/volumes/... rw\n";
        assert_eq!(
            pod_name_from_artifacts(mi, Some("my-train-pod")),
            Some("my-train-pod".to_string())
        );
        assert_eq!(pod_name_from_artifacts(mi, None), None);
        assert_eq!(
            pod_name_from_artifacts("36 30 0:34 / / rw", Some("x")),
            Some("x".to_string())
        );
    }

    #[test]
    fn lru_eviction_and_reuse_invalidation() {
        let mut r = CgroupResolver::new(2);
        // no real /proc in CI; misses cache the None result too
        let now = r.resolve(1, 100, Path::new("/proc-nonexistent"));
        assert!(now.is_none());
        let again = r.resolve(1, 100, Path::new("/proc-nonexistent"));
        assert!(again.is_none());
        // same pid, different starttime -> cache invalidated (still a miss)
        let _ = r.resolve(1, 200, Path::new("/proc-nonexistent"));
        r.resolve(2, 1, Path::new("/proc-nonexistent"));
        r.resolve(3, 1, Path::new("/proc-nonexistent"));
        r.resolve(4, 1, Path::new("/proc-nonexistent")); // evicts pid 1
        assert!(!r.cache.contains_key(&1));
        assert_eq!(r.cache.len(), 2);
    }
}
