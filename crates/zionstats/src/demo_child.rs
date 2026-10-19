//! One-command demo: spawn `zionstats --daemon --demo` as a child of this
//! process (re-exec via /proc/self/exe) so a fresh clone renders live in
//! one command. Orphan-proofing: the child runs in its own process group
//! (no double SIGINT from the terminal) with PR_SET_PDEATHSIG=SIGTERM, so
//! it can never outlive the TUI -- even on SIGKILL.

use std::io::ErrorKind;
use std::process::{Child, Command, Stdio};

pub struct DemoChild {
    child: Child,
}

impl DemoChild {
    /// SIGTERM the child, wait, done. Idempotent.
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn spawn(config_path: Option<&std::path::Path>, readonly: bool) -> Result<DemoChild, String> {
    spawn_with_args(config_path, readonly, &[])
}

/// Spawn the embedded daemon with extra CLI arguments (replay uses
/// `--replay FILE`).
pub fn spawn_with_args(
    config_path: Option<&std::path::Path>,
    readonly: bool,
    extra: &[&str],
) -> Result<DemoChild, String> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon");
    if extra.is_empty() {
        cmd.arg("--demo");
    }
    cmd.args(extra);
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(p) = config_path {
        cmd.arg("--config").arg(p);
    }
    if readonly {
        cmd.arg("--readonly");
    }
    // parent-death signal: kernel SIGTERMs the demo daemon if we die
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0);
            Ok(())
        });
    }
    cmd.spawn().map(|child| DemoChild { child }).map_err(|e| {
        if e.kind() == ErrorKind::PermissionDenied {
            format!("cannot re-exec {e}")
        } else {
            e.to_string()
        }
    })
}

/// Wait until the daemon publishes its first frame (or the child exits).
pub fn wait_for_first_frame(child: &mut DemoChild, budget_ms: u64) -> Result<bool, String> {
    use zionstats_core::shm::ShmReader;
    let uid = unsafe { libc::getuid() };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
    while std::time::Instant::now() < deadline {
        if let Ok(false) = child.child.try_wait().map(|st| st.is_some()) {
            // still running
        } else {
            return Ok(false); // child exited (most likely: another writer holds the lock)
        }
        if let Ok(frame) = ShmReader::attach(uid) {
            if frame.read().map(|f| f.header.tick_id > 0).unwrap_or(false) {
                return Ok(true);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(false)
}
