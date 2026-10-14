//! /proc parsers — pure functions, never panic on malformed input.

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcStat {
    pub ppid: i32,
    /// field 22 — PID-reuse defense key
    pub starttime: u64,
    pub utime: u64,
    pub stime: u64,
}

/// Parse /proc/<pid>/stat. The comm field can contain spaces and parens,
/// so split on the last `") "` first (Python `parse_stat_ppid` parity).
pub fn parse_stat(bytes: &[u8]) -> Option<ProcStat> {
    let text = String::from_utf8_lossy(bytes);
    let after = text.rsplit(") ").next()?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    // after `") "` split: fields start at stat field 3 (state)
    if fields.len() < 20 {
        return None;
    }
    Some(ProcStat {
        ppid: fields[1].parse().ok()?,
        utime: fields[11].parse().ok()?,
        stime: fields[12].parse().ok()?,
        starttime: fields[19].parse().ok()?,
    })
}

/// /proc/<pid>/cmdline: NUL-separated argv joined with spaces.
pub fn parse_cmdline(bytes: &[u8]) -> String {
    bytes
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p))
        .collect::<Vec<_>>()
        .join(" ")
}

/// /proc/<pid>/environ: NUL-separated KEY=VALUE pairs, sorted by key.
pub fn parse_environ(bytes: &[u8], limit: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in bytes.split(|&b| b == 0).take(limit / 32 + 1) {
        if chunk.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(chunk);
        if let Some((k, v)) = text.split_once('=') {
            if !k.is_empty() {
                out.push((k.to_string(), v.to_string()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn uid_to_name(uid: u32) -> String {
    // no passwd lookups without libc getpwuid_r plumbing here; the daemon
    // crate resolves names via its own helper (cached). Numeric fallback
    // keeps this function pure and total.
    uid.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str =
        "184722 (python) S 184700 184722 1 0 -1 4194560 0 0 0 0 150 50 0 0 20 5 0 0 1234 0";

    #[test]
    fn stat_parse() {
        let s = parse_stat(STAT.as_bytes()).unwrap();
        assert_eq!(s.ppid, 184700);
        assert_eq!(s.utime, 150);
        assert_eq!(s.stime, 50);
        assert_eq!(s.starttime, 1234);
    }

    #[test]
    fn stat_weird_comm() {
        let s = parse_stat(b"99 (my (weird) proc) R 42 99 1 0 0 0 0 0 0 0 0 0 0 0 20 5 0 0 5 0");
        assert_eq!(s.unwrap().ppid, 42);
    }

    #[test]
    fn stat_garbage_is_none() {
        assert!(parse_stat(b"garbage").is_none());
        assert!(parse_stat(b"12 (x) Z").is_none());
        assert!(parse_stat(b"").is_none());
    }

    #[test]
    fn cmdline_join() {
        assert_eq!(
            parse_cmdline(b"python\0train.py\0-b\x00256\0".as_slice()),
            "python train.py -b 256"
        );
        assert_eq!(parse_cmdline(b""), "");
        assert_eq!(parse_cmdline(b"\0\0"), "");
    }

    #[test]
    fn environ_parse_sorted() {
        let env = parse_environ(
            b"USER=demo\0CUDA_VISIBLE_DEVICES=0,1\0SHELL=/bin/bash\0",
            256 << 10,
        );
        let keys: Vec<_> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["CUDA_VISIBLE_DEVICES", "SHELL", "USER"]);
        assert_eq!(env[0].1, "0,1");
    }
}
