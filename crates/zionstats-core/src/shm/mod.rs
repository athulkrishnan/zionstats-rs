//! Shared-memory transport: create (daemon), attach (readers), and the
//! seqlock publish/read protocol. The daemon is the sole writer (guarded
//! by an advisory flock); readers never block and never take kernel locks.

pub mod layout;

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicU64, Ordering};

use layout::*;
use zerocopy::FromBytes;

#[derive(Debug, thiserror::Error)]
pub enum ShmError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("shm file not found: {0}")]
    NotFound(PathBuf),
    #[error("shm mismatch: {0}")]
    Mismatch(String),
}

pub fn shm_path(uid: u32) -> PathBuf {
    PathBuf::from(format!("/dev/shm/zionstats-{uid}"))
}

fn header_mut(map: &mut memmap2::MmapMut) -> &mut ShmHeaderV1 {
    ShmHeaderV1::mut_from_bytes(&mut map[..HDR_SIZE]).expect("header size checked")
}

fn seq_atomic_mut(map: &mut memmap2::MmapMut) -> &AtomicU64 {
    unsafe { AtomicU64::from_ptr(&mut header_mut(map).seqlock as *mut u64) }
}

fn flock_exclusive(file: &File) -> io::Result<bool> {
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    Ok(rc == 0)
}

/// Writer-side handle. Holds the File so the flock survives.
pub struct ShmWriter {
    /// held for the flock lifetime; not otherwise read
    #[allow(dead_code)]
    file: File,
    map: memmap2::MmapMut,
    path: PathBuf,
}

impl ShmWriter {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Test/bench constructor over an arbitrary path (doc-hidden: the
    /// daemon always uses `create`). Takes the flock like `create` does.
    #[doc(hidden)]
    pub fn create_at(path: &Path) -> Result<ShmWriter, ShmError> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let md = file.metadata()?;
        if !md.file_type().is_file() {
            return Err(ShmError::Mismatch(format!(
                "{}: not a regular file (symlink or special node?)",
                path.display()
            )));
        }
        if md.len() != TOTAL_SIZE as u64 {
            file.set_len(TOTAL_SIZE as u64)?;
        }
        if !flock_exclusive(&file)? {
            return Err(ShmError::Mismatch(format!(
                "another zionstats writer holds {}",
                path.display()
            )));
        }
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        *header_mut(&mut map) = ShmHeaderV1 {
            magic: *SHM_MAGIC,
            abi_tag: ABI_TAG,
            layout_version: LAYOUT_VERSION,
            total_size: TOTAL_SIZE as u64,
            seqlock: 0,
            daemon_pid: std::process::id(),
            uid: 0,
            start_ms: now_ms(),
            tick_id: 0,
            gpu_count: 0,
            proc_count: 0,
            interval_ms: 0,
            flags: 0,
            _pad: [0; 56],
        };
        Ok(ShmWriter {
            file,
            map,
            path: path.to_path_buf(),
        })
    }

    /// Create (or reclaim) the shm file for `uid` and take the advisory
    /// writer lock. Mode 0600: same-uid readers only, unless the daemon
    /// chown/chmods to `shm_group` afterwards.
    pub fn create(uid: u32) -> Result<ShmWriter, ShmError> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::OpenOptionsExt;
        let path = shm_path(uid);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let md = file.metadata()?;
        if !md.file_type().is_file() {
            return Err(ShmError::Mismatch(format!(
                "{}: not a regular file (symlink or special node?)",
                path.display()
            )));
        }
        if md.uid() != uid {
            return Err(ShmError::Mismatch(format!(
                "{}: owned by uid {}, expected {}",
                path.display(),
                md.uid(),
                uid
            )));
        }
        if md.len() != TOTAL_SIZE as u64 {
            file.set_len(TOTAL_SIZE as u64)?;
        }
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = file.metadata()?.permissions();
            perms.set_mode(0o600);
            file.set_permissions(perms)?;
        }
        // advisory exclusive lock: refuses a second daemon (including the
        // Python --log controller racing us for the same plug)
        if !flock_exclusive(&file)? {
            return Err(ShmError::Mismatch(format!(
                "another zionstats writer holds {}",
                path.display()
            )));
        }
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        *header_mut(&mut map) = ShmHeaderV1 {
            magic: *SHM_MAGIC,
            abi_tag: ABI_TAG,
            layout_version: LAYOUT_VERSION,
            total_size: TOTAL_SIZE as u64,
            seqlock: 0,
            daemon_pid: std::process::id(),
            uid,
            start_ms: now_ms(),
            tick_id: 0,
            gpu_count: 0,
            proc_count: 0,
            interval_ms: 0,
            flags: 0,
            _pad: [0; 56],
        };
        Ok(ShmWriter { file, map, path })
    }

    /// Seqlock publish: seq odd -> payload -> seq even. The closure fills
    /// payload regions between the two fences.
    pub fn publish<F: FnOnce(&mut ShmPayloadView)>(&mut self, interval_ms: u32, fill: F) {
        // Canonical seqlock fencing (Boehm 2014), hardened in two places
        // after the multi-reader test caught torn frames:
        //  - fence(SeqCst) after the Relaxed odd store forbids payload
        //    writes hoisting above it (Release fence is not sufficient);
        //  - the even store is Release so payload plain-stores cannot sink
        //    past it (a Relaxed even store was observed reordering).
        let s = {
            let seq = seq_atomic_mut(&mut self.map);
            let s = seq.load(Ordering::Relaxed);
            seq.store(s.wrapping_add(1), Ordering::Relaxed);
            s + 1
        };
        fence(Ordering::SeqCst);

        {
            let header = header_mut(&mut self.map);
            header.tick_id = header.tick_id.wrapping_add(1);
            header.interval_ms = interval_ms;
        }
        let mut view = ShmPayloadView { map: &mut self.map };
        fill(&mut view);

        let seq = seq_atomic_mut(&mut self.map);
        seq.store(s.wrapping_add(1), Ordering::Release);
    }

    /// Advisory metadata (readonly/demo flags) — written between ticks.
    pub fn set_flags(&mut self, flags: u32) {
        header_mut(&mut self.map).flags = flags;
    }
}

/// Writer-side view passed to the publish closure.
pub struct ShmPayloadView<'a> {
    map: &'a mut memmap2::MmapMut,
}

impl ShmPayloadView<'_> {
    pub fn header(&mut self) -> &mut ShmHeaderV1 {
        header_mut(self.map)
    }

    pub fn gpu_mut(&mut self, idx: usize) -> Option<&mut GpuSampleV1> {
        if idx >= MAX_GPUS {
            return None;
        }
        let off = GPUS_OFF + idx * GPU_STRIDE;
        GpuSampleV1::mut_from_bytes(&mut self.map[off..off + GPU_STRIDE]).ok()
    }

    pub fn proc_mut(&mut self, idx: usize) -> Option<&mut ProcEntryV1> {
        if idx >= MAX_PROCS {
            return None;
        }
        let off = PROCS_OFF + idx * PROC_STRIDE;
        ProcEntryV1::mut_from_bytes(&mut self.map[off..off + PROC_STRIDE]).ok()
    }

    pub fn control_mut(&mut self) -> &mut ControlStateV1 {
        ControlStateV1::mut_from_bytes(&mut self.map[CTRL_OFF..CTRL_OFF + CTRL_SIZE])
            .expect("control region size is compile-time checked")
    }

    pub fn ring_mut(&mut self, gpu: usize) -> Option<&mut HistoryRingV1> {
        if gpu >= MAX_GPUS {
            return None;
        }
        let off = RINGS_OFF + gpu * RING_SIZE;
        HistoryRingV1::mut_from_bytes(&mut self.map[off..off + RING_SIZE]).ok()
    }

    pub fn alert_mut(&mut self, idx: usize) -> Option<&mut AlertV1> {
        if idx >= MAX_ALERTS {
            return None;
        }
        let off = ALERTS_OFF + idx * ALERT_SIZE;
        AlertV1::mut_from_bytes(&mut self.map[off..off + ALERT_SIZE]).ok()
    }

    pub fn insight_mut(&mut self, idx: usize) -> Option<&mut InsightBlockV1> {
        InsightBlockV1::mut_from_bytes(
            &mut self.map
                [INSIGHTS_OFF + idx * INSIGHT_STRIDE..INSIGHTS_OFF + (idx + 1) * INSIGHT_STRIDE],
        )
        .ok()
    }

    pub fn env_mut(&mut self) -> &mut EnvBlockV1 {
        EnvBlockV1::mut_from_bytes(&mut self.map[ENV_OFF..ENV_OFF + ENV_SIZE])
            .expect("env region size is compile-time checked")
    }
}

/// Reader-side handle (read-only mapping).
pub struct ShmReader {
    map: memmap2::Mmap,
    path: PathBuf,
}

impl ShmReader {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn attach(uid: u32) -> Result<ShmReader, ShmError> {
        Self::attach_path(&shm_path(uid))
    }

    pub fn attach_path(path: &Path) -> Result<ShmReader, ShmError> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|_| ShmError::NotFound(path.to_path_buf()))
            .and_then(|f| {
                let len_ok = f
                    .metadata()
                    .map(|m| m.len() == TOTAL_SIZE as u64)
                    .unwrap_or(false);
                if len_ok {
                    Ok(f)
                } else {
                    Err(ShmError::Mismatch(format!(
                        "{}: size != {TOTAL_SIZE}",
                        path.display()
                    )))
                }
            })?;
        let map = unsafe { memmap2::Mmap::map(&file)? };
        let reader = ShmReader {
            map,
            path: path.to_path_buf(),
        };
        reader.validate()?;
        Ok(reader)
    }

    fn validate(&self) -> Result<(), ShmError> {
        let h = self.header();
        if h.magic != *SHM_MAGIC {
            return Err(ShmError::Mismatch("bad magic".into()));
        }
        if h.abi_tag != ABI_TAG {
            return Err(ShmError::Mismatch(format!("abi_tag {:#x}", h.abi_tag)));
        }
        if h.layout_version != LAYOUT_VERSION {
            return Err(ShmError::Mismatch(format!("layout {}", h.layout_version)));
        }
        if h.total_size != TOTAL_SIZE as u64 {
            return Err(ShmError::Mismatch(format!("total_size {}", h.total_size)));
        }
        Ok(())
    }

    fn header(&self) -> &ShmHeaderV1 {
        ShmHeaderV1::ref_from_bytes(&self.map[..HDR_SIZE]).expect("header size checked")
    }

    fn seq_atomic(&self) -> &AtomicU64 {
        unsafe { AtomicU64::from_ptr(&self.header().seqlock as *const u64 as *mut u64) }
    }

    /// Bounded-retry seqlock read. On exhaustion returns the last copy
    /// with `stale = true` (readers show a staleness badge, never fail).
    pub fn read(&self) -> Result<OwnedFrame, ShmError> {
        const RETRIES: usize = 8;
        let seq = self.seq_atomic();
        for attempt in 0..RETRIES {
            let s1 = seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                std::hint::spin_loop();
                if attempt >= RETRIES / 2 {
                    std::thread::yield_now();
                }
                continue;
            }
            fence(Ordering::Acquire);
            let frame = self.copy_frame();
            fence(Ordering::SeqCst);
            let s2 = seq.load(Ordering::Acquire);
            if s1 == s2 {
                return Ok(frame);
            }
        }
        let mut frame = self.copy_frame();
        frame.stale = true;
        Ok(frame)
    }

    fn copy_frame(&self) -> OwnedFrame {
        let h = *self.header();
        let gpu_count = (h.gpu_count as usize).min(MAX_GPUS);
        let proc_count = (h.proc_count as usize).min(MAX_PROCS);
        let mut gpus = Vec::with_capacity(gpu_count);
        for i in 0..gpu_count {
            let off = GPUS_OFF + i * GPU_STRIDE;
            if let Ok(g) = GpuSampleV1::ref_from_bytes(&self.map[off..off + GPU_STRIDE]) {
                gpus.push(*g);
            }
        }
        let mut procs = Vec::with_capacity(proc_count);
        for i in 0..proc_count {
            let off = PROCS_OFF + i * PROC_STRIDE;
            if let Ok(p) = ProcEntryV1::ref_from_bytes(&self.map[off..off + PROC_STRIDE]) {
                procs.push(*p);
            }
        }
        let control = *ControlStateV1::ref_from_bytes(&self.map[CTRL_OFF..CTRL_OFF + CTRL_SIZE])
            .expect("control size checked");
        let mut rings = Vec::with_capacity(gpu_count);
        for i in 0..gpu_count {
            let off = RINGS_OFF + i * RING_SIZE;
            if let Ok(r) = HistoryRingV1::ref_from_bytes(&self.map[off..off + RING_SIZE]) {
                rings.push(*r);
            }
        }
        let mut alerts = Vec::new();
        for i in 0..MAX_ALERTS {
            let off = ALERTS_OFF + i * ALERT_SIZE;
            if let Ok(a) = AlertV1::ref_from_bytes(&self.map[off..off + ALERT_SIZE]) {
                if a.seq != 0 {
                    alerts.push(*a);
                }
            }
        }
        alerts.sort_by_key(|a| a.seq);
        let env = *EnvBlockV1::ref_from_bytes(&self.map[ENV_OFF..ENV_OFF + ENV_SIZE])
            .expect("env size checked");
        let mut insights = Vec::with_capacity(gpu_count);
        for i in 0..gpu_count {
            let off = INSIGHTS_OFF + i * INSIGHT_STRIDE;
            if let Ok(b) = InsightBlockV1::ref_from_bytes(&self.map[off..off + INSIGHT_STRIDE]) {
                insights.push(*b);
            }
        }
        OwnedFrame {
            header: h,
            gpus,
            procs,
            control,
            rings,
            alerts,
            env,
            insights,
            stale: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OwnedFrame {
    pub header: ShmHeaderV1,
    pub gpus: Vec<GpuSampleV1>,
    pub procs: Vec<ProcEntryV1>,
    pub control: ControlStateV1,
    pub rings: Vec<HistoryRingV1>,
    pub alerts: Vec<AlertV1>,
    pub env: EnvBlockV1,
    pub insights: Vec<InsightBlockV1>,
    pub stale: bool,
}

impl OwnedFrame {
    /// Age of the newest GPU sample in ms — the reader-side liveness proxy
    /// (header has no dedicated publish timestamp in layout v1).
    pub fn newest_sample_age_ms(&self) -> u64 {
        let newest = self.gpus.iter().map(|g| g.ts_ms).max().unwrap_or(0);
        now_ms().saturating_sub(newest)
    }

    /// Daemon considered down when its pid is gone or samples are older
    /// than 5 poll intervals.
    pub fn daemon_alive(&self) -> bool {
        if self.header.daemon_pid == 0 {
            return false;
        }
        let rc = unsafe { libc::kill(self.header.daemon_pid as i32, 0) };
        let pid_alive =
            rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        pid_alive && self.newest_sample_age_ms() <= 5 * u64::from(self.header.interval_ms.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;

    #[test]
    fn seqlock_torn_read_integrity_multithreaded() {
        // 1 writer + 4 readers; writer embeds the same tick id in three
        // places (header.tick_id, gpu[0].ts_ms, ring head) — readers must
        // never observe a mixed frame.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zionstats-seq");
        {
            use std::os::unix::fs::OpenOptionsExt;
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            f.set_len(TOTAL_SIZE as u64).unwrap();
        }
        let mut writer_map = unsafe {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            memmap2::MmapMut::map_mut(&f).unwrap()
        };
        *header_mut(&mut writer_map) = ShmHeaderV1 {
            magic: *SHM_MAGIC,
            abi_tag: ABI_TAG,
            layout_version: LAYOUT_VERSION,
            total_size: TOTAL_SIZE as u64,
            seqlock: 0,
            daemon_pid: std::process::id(),
            uid: 0,
            start_ms: now_ms(),
            tick_id: 0,
            gpu_count: 1,
            proc_count: 0,
            interval_ms: 1,
            flags: 0,
            _pad: [0; 56],
        };

        let stop = Arc::new(AtomicU64::new(0));
        // start gate: every reader is attached before the writer begins,
        // so a loaded CI box cannot starve readers into zero checks
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let stop = stop.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            readers.push(std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).open(&path).unwrap();
                let map = unsafe { memmap2::Mmap::map(&file).unwrap() };
                let reader = ShmReader { map, path };
                let mut checks = 0u64;
                barrier.wait();
                // a heavily loaded box can preempt a reader for the whole
                // writer run -- keep going until at least one check lands
                while stop.load(AtomicOrdering::Relaxed) == 0 || checks == 0 {
                    let frame = reader.read().unwrap();
                    // stale frames are the documented bounded-retry fallback
                    // (possibly mid-write); integrity is only guaranteed for
                    // clean frames
                    if frame.stale || frame.gpus.is_empty() {
                        continue;
                    }
                    let t = frame.header.tick_id;
                    let g = frame.gpus[0].ts_ms;
                    let r = frame.rings[0].head;
                    // t published in the same section as g and r
                    assert_eq!(t, g, "torn frame: header vs gpu");
                    assert_eq!(t, u64::from(r), "torn frame: header vs ring");
                    checks += 1;
                }
                checks
            }));
        }

        barrier.wait();
        let mut tick: u64 = 0;
        for _ in 0..50_000 {
            tick += 1;
            let s = {
                let seq = seq_atomic_mut(&mut writer_map);
                let s = seq.load(Ordering::Relaxed);
                seq.store(s.wrapping_add(1), Ordering::Relaxed);
                s + 1
            };
            fence(Ordering::SeqCst);
            {
                let header = header_mut(&mut writer_map);
                header.tick_id = tick;
                if let Ok(g) =
                    GpuSampleV1::mut_from_bytes(&mut writer_map[GPUS_OFF..GPUS_OFF + GPU_STRIDE])
                {
                    g.ts_ms = tick;
                }
                if let Ok(r) =
                    HistoryRingV1::mut_from_bytes(&mut writer_map[RINGS_OFF..RINGS_OFF + RING_SIZE])
                {
                    r.head = tick as u32;
                }
            }
            let seq = seq_atomic_mut(&mut writer_map);
            seq.store(s.wrapping_add(1), Ordering::Release);
        }

        stop.store(1, AtomicOrdering::Relaxed);
        let total: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(total > 0, "readers must have completed checks");
    }

    #[test]
    fn attach_rejects_bad_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad");
        std::fs::write(&path, vec![0u8; TOTAL_SIZE]).unwrap();
        assert!(ShmReader::attach_path(&path).is_err());
        // wrong size
        let path2 = dir.path().join("small");
        std::fs::write(&path2, vec![0u8; 1024]).unwrap();
        assert!(ShmReader::attach_path(&path2).is_err());
        // missing file
        assert!(matches!(
            ShmReader::attach_path(&dir.path().join("nope")),
            Err(ShmError::NotFound(_))
        ));
    }

    #[test]
    fn attach_rejects_v1_layout_loudly() {
        // a v1 file under a v2 reader must be a named error, never a
        // silent misparse (the strides moved in v2)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1file");
        // forge a header with the current magic/abi but layout_version 1
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        f.set_len(TOTAL_SIZE as u64).unwrap();
        let mut map = unsafe { memmap2::MmapMut::map_mut(&f).unwrap() };
        let h = ShmHeaderV1 {
            magic: *SHM_MAGIC,
            abi_tag: ABI_TAG,
            layout_version: 1,
            total_size: TOTAL_SIZE as u64,
            seqlock: 0,
            daemon_pid: std::process::id(),
            uid: 0,
            start_ms: 0,
            tick_id: 0,
            gpu_count: 0,
            proc_count: 0,
            interval_ms: 0,
            flags: 0,
            _pad: [0; 56],
        };
        let bytes = zerocopy::IntoBytes::as_bytes(&h);
        map[..bytes.len()].copy_from_slice(bytes);
        drop(map);
        drop(f);
        let err = match ShmReader::attach_path(&path) {
            Err(e) => e,
            Ok(_) => panic!("v1 file must not attach under a v2 reader"),
        };
        assert!(
            err.to_string().contains("layout 1"),
            "rejection must name the layout version, got: {err}"
        );
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn symlink_at_shm_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("zionstats-symlink");
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"important data here").unwrap();
        std::os::unix::fs::symlink(&victim, &shm).unwrap();

        let result = ShmWriter::create_at(&shm);
        assert!(result.is_err(), "symlink must be rejected");

        // victim file is unmodified
        let content = std::fs::read(&victim).unwrap();
        assert_eq!(content, b"important data here");
        assert_eq!(content.len(), 19);
    }

    #[test]
    fn directory_at_shm_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("zionstats-dir");
        std::fs::create_dir(&shm).unwrap();

        let result = ShmWriter::create_at(&shm);
        assert!(result.is_err(), "directory must be rejected");
    }

    #[test]
    fn normal_create_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zionstats-ok");
        // create the file first (create_at expects an existing file or creates one)
        let result = ShmWriter::create_at(&path);
        assert!(result.is_ok(), "normal create must succeed");
    }
}
