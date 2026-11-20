//! The rig-pose file: one writer (the daemon), many lock-free readers (the VR consumers).
//!
//! The writer holds an exclusive `flock` and publishes with a seqlock: it writes the sequence number
//! (odd), then the payload, then the sequence number (even). A reader reads the sequence number, the
//! payload and the sequence number again, each with one positional read, and accepts the payload only
//! if both sequence numbers are equal and even; otherwise it retries. Neither side ever blocks the other.
//!
//! Positional I/O (`pread`/`pwrite` through [`FileExt`]) on a tmpfs file keeps this crate free of
//! `unsafe`; each call is one copy into or out of the page cache. The three separate reads are ordered
//! by the system calls between them.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::layout::{
    self, HeaderCheck, CONSUMER_LEN, CONSUMER_OFFSET, FILE_SIZE, HEADER_LEN, PAYLOAD_LEN, PAYLOAD_OFFSET, SEQ_OFFSET,
};
use crate::sample::RigSample;

/// Where the daemon publishes the rig pose unless `OPENRIGCUE_RIG_POSE` names another file.
pub const DEFAULT_PATH: &str = "/dev/shm/openrigcue/rig-pose";

/// The environment variable that overrides [`DEFAULT_PATH`] for every reader and writer.
pub const PATH_ENV: &str = "OPENRIGCUE_RIG_POSE";

/// A consumer claim whose heartbeat is older than this is free to take.
pub const CLAIM_TIMEOUT_NS: u64 = 500_000_000;

/// Suffix of the lock file next to the pose file (`rig-pose.claim`) that serialises the consumers'
/// read-decide-write of the claim. The pose file itself cannot be used: the writer holds its `flock`.
pub const CLAIM_LOCK_SUFFIX: &str = ".claim";

/// How long [`RigPoseReader::claim`] waits for the claim lock before deciding without it. The lock is
/// held for two positional I/O calls, so this is only reached if a holder is stopped or stuck.
const CLAIM_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(2);

/// The claim lock file for the pose file at `path`.
pub fn claim_lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(CLAIM_LOCK_SUFFIX);
    PathBuf::from(p)
}

/// An exclusive `flock` on the claim lock file, released on drop.
struct ClaimLock<'a>(&'a File);

impl<'a> ClaimLock<'a> {
    /// Takes the lock, waiting at most `wait`; `None` if it could not be taken.
    fn acquire(file: &'a File, wait: std::time::Duration) -> Option<Self> {
        use rustix::fs::{flock, FlockOperation};
        let deadline = std::time::Instant::now() + wait;
        loop {
            match flock(file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Some(ClaimLock(file)),
                Err(rustix::io::Errno::WOULDBLOCK | rustix::io::Errno::INTR)
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_micros(20));
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for ClaimLock<'_> {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(self.0, rustix::fs::FlockOperation::Unlock);
    }
}

/// How many times [`RigPoseReader::read`] retries a read that overlapped a write.
const READ_RETRIES: usize = 64;

/// The rig-pose path: `$OPENRIGCUE_RIG_POSE` if set and non-empty, otherwise [`DEFAULT_PATH`].
pub fn default_path() -> PathBuf {
    match std::env::var_os(PATH_ENV) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_PATH),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RigPoseError {
    #[error("rig-pose file {0} does not exist")]
    Missing(PathBuf),
    #[error("rig-pose file {path} is already being written by another process")]
    AlreadyLocked { path: PathBuf },
    #[error("rig-pose file {path} is not a valid version-{} file ({check:?})", layout::VERSION)]
    BadHeader { path: PathBuf, check: HeaderCheck },
    #[error("rig-pose file {path} is too short ({len} bytes)")]
    TooShort { path: PathBuf, len: u64 },
    #[error("rig-pose file kept changing during {READ_RETRIES} read attempts")]
    Busy,
    #[error("rig-pose file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl RigPoseError {
    fn io(path: &Path, source: io::Error) -> Self {
        if source.kind() == io::ErrorKind::NotFound {
            RigPoseError::Missing(path.to_owned())
        } else {
            RigPoseError::Io { path: path.to_owned(), source }
        }
    }
}

/// The publishing side, owned by the daemon.
#[derive(Debug)]
pub struct RigPoseWriter {
    file: File,
    path: PathBuf,
    seq: u64,
    pid: u32,
    last: Option<RigSample>,
}

impl RigPoseWriter {
    /// Opens or creates the file (and its directory, mode 0700; the file is mode 0600), takes the
    /// exclusive writer lock and initialises the header if it is missing or from another layout.
    ///
    /// An existing valid file is reused in place, never truncated or replaced, so readers that already
    /// have it open keep working across daemon restarts.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, RigPoseError> {
        let path = path.as_ref();
        let err = |e| RigPoseError::io(path, e);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(err)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(err)?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(RigPoseError::AlreadyLocked { path: path.to_owned() }),
            Err(e) => return Err(err(e.into())),
        }

        let mut header = [0u8; HEADER_LEN];
        let len = file.metadata().map_err(err)?.len();
        let valid = len == u64::from(FILE_SIZE)
            && file.read_exact_at(&mut header, 0).is_ok()
            && layout::check_header(&header) == HeaderCheck::Valid;
        if !valid {
            file.set_len(0).map_err(err)?;
            file.set_len(u64::from(FILE_SIZE)).map_err(err)?;
            file.write_all_at(&layout::encode_header(), 0).map_err(err)?;
        }
        let mut seq_bytes = [0u8; 8];
        file.read_exact_at(&mut seq_bytes, SEQ_OFFSET).map_err(err)?;
        let mut seq = u64::from_le_bytes(seq_bytes);
        if seq % 2 == 1 {
            // A previous writer died mid-write; make the sequence even (and different) again.
            seq = seq.wrapping_add(1);
            file.write_all_at(&seq.to_le_bytes(), SEQ_OFFSET).map_err(err)?;
        }
        Ok(RigPoseWriter { file, path: path.to_owned(), seq, pid: std::process::id(), last: None })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Publishes one sample. Every call is written, including an all-zero (neutral) pose.
    pub fn publish(&mut self, sample: &RigSample) -> Result<(), RigPoseError> {
        let payload = layout::encode_payload(sample, self.pid);
        let begin = self.seq.wrapping_add(1);
        let end = self.seq.wrapping_add(2);
        let err = |e| RigPoseError::io(&self.path, e);
        self.file.write_all_at(&begin.to_le_bytes(), SEQ_OFFSET).map_err(err)?;
        self.file.write_all_at(&payload, PAYLOAD_OFFSET).map_err(err)?;
        self.file.write_all_at(&end.to_le_bytes(), SEQ_OFFSET).map_err(err)?;
        self.seq = end;
        self.last = Some(*sample);
        Ok(())
    }
}

impl Drop for RigPoseWriter {
    /// On a clean shutdown the last pose is republished with compensation disabled, so consumers pass
    /// through at once instead of waiting for the sample to go stale.
    fn drop(&mut self) {
        if let Some(last) = self.last {
            let _ = self.publish(&RigSample { enabled: false, ..last });
        }
    }
}

/// One consistent read of the payload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub sample: RigSample,
    pub writer_pid: u32,
    /// The (even) seqlock sequence number of this snapshot; it increases by 2 per publish.
    pub sequence: u64,
}

/// Who may compensate. A higher code outranks a lower one: on SteamVR the vrserver driver compensates
/// every application and the compositor, so an OpenXR layer running on SteamVR must yield to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConsumerKind(pub u32);

impl ConsumerKind {
    /// The OpenXR API layer (`openrigcue-xr-layer`).
    pub const XR_LAYER: ConsumerKind = ConsumerKind(1);
    /// The SteamVR (vrserver) driver.
    pub const STEAMVR_DRIVER: ConsumerKind = ConsumerKind(2);

    pub fn name(self) -> &'static str {
        match self {
            ConsumerKind::XR_LAYER => "openxr-layer",
            ConsumerKind::STEAMVR_DRIVER => "steamvr-driver",
            _ => "unknown",
        }
    }
}

/// The current holder of the active-consumer field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerClaim {
    pub kind: ConsumerKind,
    pub pid: u32,
    pub heartbeat_ns: u64,
}

impl ConsumerClaim {
    /// True while the holder's heartbeat is recent at `now_ns`. A heartbeat further in the future than
    /// the timeout is treated as garbage.
    pub fn is_live(&self, now_ns: u64) -> bool {
        now_ns.abs_diff(self.heartbeat_ns) <= CLAIM_TIMEOUT_NS
    }
}

/// The result of [`RigPoseReader::claim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller may compensate. `recorded` is false when the file is read-only to this process, so
    /// other consumers cannot see the claim.
    Granted { recorded: bool },
    /// A live, equal-or-higher-ranked consumer of another kind holds the claim; the caller must pass through.
    Yielded(ConsumerClaim),
}

/// Whether a consumer of `kind` may take or keep the claim currently held by `holder`.
pub fn may_claim(holder: Option<ConsumerClaim>, kind: ConsumerKind, now_ns: u64) -> bool {
    match holder {
        None => true,
        Some(h) => !h.is_live(now_ns) || h.kind <= kind,
    }
}

/// The reading side, used by the VR consumers. All methods take `&self` and never block the writer.
#[derive(Debug)]
pub struct RigPoseReader {
    file: File,
    path: PathBuf,
    writable: bool,
    pid: u32,
    /// The claim lock file ([`claim_lock_path`]), when it could be opened.
    claim_lock: Option<File>,
}

impl RigPoseReader {
    /// Opens an existing file and checks its header. It opens read-write when permitted (to take part
    /// in the consumer claim) and falls back to read-only.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RigPoseError> {
        let path = path.as_ref();
        let err = |e| RigPoseError::io(path, e);
        let (file, writable) = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => (f, true),
            Err(e) if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem) => {
                (File::open(path).map_err(err)?, false)
            }
            Err(e) => return Err(err(e)),
        };
        let len = file.metadata().map_err(err)?.len();
        if len < u64::from(FILE_SIZE) {
            return Err(RigPoseError::TooShort { path: path.to_owned(), len });
        }
        let mut header = [0u8; HEADER_LEN];
        file.read_exact_at(&mut header, 0).map_err(err)?;
        match layout::check_header(&header) {
            HeaderCheck::Valid => Ok(RigPoseReader {
                file,
                path: path.to_owned(),
                writable,
                pid: std::process::id(),
                // Only consumers that can record a claim take part in the lock.
                claim_lock: writable
                    .then(|| {
                        OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .truncate(false)
                            .mode(0o600)
                            .open(claim_lock_path(path))
                            .ok()
                    })
                    .flatten(),
            }),
            check => Err(RigPoseError::BadHeader { path: path.to_owned(), check }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this reader can write the consumer claim.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// A consistent snapshot of the payload, retrying while a write is in progress.
    pub fn read(&self) -> Result<Snapshot, RigPoseError> {
        let err = |e| RigPoseError::io(&self.path, e);
        let mut payload = [0u8; PAYLOAD_LEN];
        for _ in 0..READ_RETRIES {
            let before = self.read_seq().map_err(err)?;
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            self.file.read_exact_at(&mut payload, PAYLOAD_OFFSET).map_err(err)?;
            if self.read_seq().map_err(err)? == before {
                let (sample, writer_pid) = layout::decode_payload(&payload);
                return Ok(Snapshot { sample, writer_pid, sequence: before });
            }
            std::hint::spin_loop();
        }
        Err(RigPoseError::Busy)
    }

    fn read_seq(&self) -> io::Result<u64> {
        let mut b = [0u8; 8];
        self.file.read_exact_at(&mut b, SEQ_OFFSET)?;
        Ok(u64::from_le_bytes(b))
    }

    /// The current active-consumer claim, if any.
    pub fn consumer(&self) -> Result<Option<ConsumerClaim>, RigPoseError> {
        let mut b = [0u8; CONSUMER_LEN];
        self.file.read_exact_at(&mut b, CONSUMER_OFFSET).map_err(|e| RigPoseError::io(&self.path, e))?;
        let (kind, pid, heartbeat_ns) = layout::decode_consumer(&b);
        Ok((kind != 0).then_some(ConsumerClaim { kind: ConsumerKind(kind), pid, heartbeat_ns }))
    }

    /// Takes or refreshes the active-consumer claim for `kind` at `now_ns`, unless a live consumer
    /// of a higher kind holds it. Call it once per frame while compensating; the heartbeat expires
    /// after [`CLAIM_TIMEOUT_NS`].
    ///
    /// Consumers of the same kind (for example two OpenXR applications) share the claim.
    ///
    /// The read-decide-write runs under an exclusive `flock` on [`claim_lock_path`], so a
    /// lower-ranked consumer can never overwrite a higher-ranked claim written between its read and
    /// its write. If the lock file cannot be opened, or the lock is not free within a few
    /// milliseconds (a holder stopped mid-claim), the claim is decided without it.
    pub fn claim(&self, kind: ConsumerKind, now_ns: u64) -> Result<ClaimOutcome, RigPoseError> {
        self.claim_waiting(kind, now_ns, CLAIM_LOCK_WAIT)
    }

    fn claim_waiting(
        &self,
        kind: ConsumerKind,
        now_ns: u64,
        wait: std::time::Duration,
    ) -> Result<ClaimOutcome, RigPoseError> {
        let _lock = self.lock_claims(wait);
        let holder = self.consumer()?;
        if !may_claim(holder, kind, now_ns) {
            return Ok(ClaimOutcome::Yielded(holder.expect("may_claim(None) is true")));
        }
        if !self.writable {
            return Ok(ClaimOutcome::Granted { recorded: false });
        }
        let b = layout::encode_consumer(kind.0, self.pid, now_ns);
        self.file.write_all_at(&b, CONSUMER_OFFSET).map_err(|e| RigPoseError::io(&self.path, e))?;
        Ok(ClaimOutcome::Granted { recorded: true })
    }

    fn lock_claims(&self, wait: std::time::Duration) -> Option<ClaimLock<'_>> {
        self.claim_lock.as_ref().and_then(|f| ClaimLock::acquire(f, wait))
    }

    /// Clears the claim if this process holds it for `kind`.
    pub fn release(&self, kind: ConsumerKind) -> Result<(), RigPoseError> {
        if !self.writable {
            return Ok(());
        }
        let _lock = self.lock_claims(CLAIM_LOCK_WAIT);
        match self.consumer()? {
            Some(h) if h.kind == kind && h.pid == self.pid => self
                .file
                .write_all_at(&[0u8; CONSUMER_LEN], CONSUMER_OFFSET)
                .map_err(|e| RigPoseError::io(&self.path, e)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{CorOffset, RigValues};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn sample(i: u64) -> RigSample {
        let v = i as f64;
        RigSample {
            timestamp_ns: 1 + i,
            values: RigValues { sway: v, surge: v, heave: v, yaw: v, roll: v, pitch: v },
            rates: Some(RigValues::from_array([v; 6])),
            cor_offset: CorOffset { forward: v, down: v, right: v },
            enabled: true,
            recenter_generation: i as u32,
        }
    }

    #[test]
    fn write_then_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/rig-pose");
        let mut w = RigPoseWriter::create(&path).unwrap();
        let r = RigPoseReader::open(&path).unwrap();
        assert_eq!(r.read().unwrap().sample.timestamp_ns, 0, "fresh file reads as never written");
        w.publish(&sample(5)).unwrap();
        let snap = r.read().unwrap();
        assert_eq!(snap.sample, sample(5));
        assert_eq!(snap.writer_pid, std::process::id());
        assert_eq!(snap.sequence, 2);
        // A neutral (all-zero) pose is published too.
        w.publish(&RigSample { values: RigValues::ZERO, ..sample(6) }).unwrap();
        assert_eq!(r.read().unwrap().sample.values, RigValues::ZERO);
    }

    #[test]
    fn file_and_directory_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openrigcue/rig-pose");
        let _w = RigPoseWriter::create(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), u64::from(FILE_SIZE));
    }

    #[test]
    fn only_one_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        let _w = RigPoseWriter::create(&path).unwrap();
        assert!(matches!(RigPoseWriter::create(&path), Err(RigPoseError::AlreadyLocked { .. })));
    }

    #[test]
    fn restarted_writer_reuses_the_file_and_readers_keep_working() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        let mut w = RigPoseWriter::create(&path).unwrap();
        w.publish(&sample(1)).unwrap();
        let r = RigPoseReader::open(&path).unwrap();
        drop(w);
        // Clean shutdown republishes the last pose disabled.
        let snap = r.read().unwrap();
        assert!(!snap.sample.enabled);
        assert_eq!(snap.sample.values, sample(1).values);
        let mut w = RigPoseWriter::create(&path).unwrap();
        w.publish(&sample(2)).unwrap();
        let snap = r.read().unwrap();
        assert_eq!(snap.sample, sample(2));
        assert!(snap.sequence > 4);
    }

    #[test]
    fn odd_sequence_from_a_crashed_writer_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        drop(RigPoseWriter::create(&path).unwrap());
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all_at(&7u64.to_le_bytes(), SEQ_OFFSET).unwrap();
        let r = RigPoseReader::open(&path).unwrap();
        assert!(matches!(r.read(), Err(RigPoseError::Busy)));
        let _w = RigPoseWriter::create(&path).unwrap();
        assert_eq!(r.read().unwrap().sequence, 8);
    }

    #[test]
    fn garbage_file_is_reinitialised_by_the_writer_and_rejected_by_readers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        std::fs::write(&path, vec![0xAAu8; 300]).unwrap();
        assert!(matches!(RigPoseReader::open(&path), Err(RigPoseError::BadHeader { .. })));
        std::fs::write(&path, b"short").unwrap();
        assert!(matches!(RigPoseReader::open(&path), Err(RigPoseError::TooShort { .. })));
        let _w = RigPoseWriter::create(&path).unwrap();
        assert!(RigPoseReader::open(&path).is_ok());
    }

    #[test]
    fn missing_file_is_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(RigPoseReader::open(dir.path().join("nope")), Err(RigPoseError::Missing(_))));
    }

    #[test]
    fn concurrent_reads_never_see_a_torn_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        let mut w = RigPoseWriter::create(&path).unwrap();
        w.publish(&sample(0)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let path = path.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let r = RigPoseReader::open(&path).unwrap();
                    let mut good = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        match r.read() {
                            Ok(s) => {
                                // Every field of sample(i) is derived from the same i.
                                let i = s.sample.timestamp_ns - 1;
                                assert_eq!(s.sample, sample(i), "torn read");
                                good += 1;
                            }
                            Err(RigPoseError::Busy) => {}
                            Err(e) => panic!("{e}"),
                        }
                    }
                    good
                })
            })
            .collect();
        for i in 1..20_000 {
            w.publish(&sample(i)).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
    }

    #[test]
    fn consumer_claims_rank_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        let _w = RigPoseWriter::create(&path).unwrap();
        let layer = RigPoseReader::open(&path).unwrap();
        let driver = RigPoseReader::open(&path).unwrap();
        let t = 10_000_000_000u64;

        assert_eq!(layer.consumer().unwrap(), None);
        assert_eq!(layer.claim(ConsumerKind::XR_LAYER, t).unwrap(), ClaimOutcome::Granted { recorded: true });
        // The driver outranks the layer and takes over a live layer claim.
        assert_eq!(
            driver.claim(ConsumerKind::STEAMVR_DRIVER, t + 1).unwrap(),
            ClaimOutcome::Granted { recorded: true }
        );
        let held = match layer.claim(ConsumerKind::XR_LAYER, t + 2).unwrap() {
            ClaimOutcome::Yielded(h) => h,
            other => panic!("{other:?}"),
        };
        assert_eq!(held.kind, ConsumerKind::STEAMVR_DRIVER);
        assert_eq!(held.heartbeat_ns, t + 1);
        // Once the driver's heartbeat expires the layer may take over.
        let later = t + 1 + CLAIM_TIMEOUT_NS + 1;
        assert_eq!(layer.claim(ConsumerKind::XR_LAYER, later).unwrap(), ClaimOutcome::Granted { recorded: true });
        // Release only clears our own claim.
        driver.release(ConsumerKind::STEAMVR_DRIVER).unwrap();
        assert_eq!(layer.consumer().unwrap().map(|c| c.kind), Some(ConsumerKind::XR_LAYER));
        layer.release(ConsumerKind::XR_LAYER).unwrap();
        assert_eq!(layer.consumer().unwrap(), None);
    }

    #[test]
    fn claims_are_serialised_by_the_claim_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        let _w = RigPoseWriter::create(&path).unwrap();
        let layer = RigPoseReader::open(&path).unwrap();
        let driver = RigPoseReader::open(&path).unwrap();
        assert!(claim_lock_path(&path).exists());
        let t = 10_000_000_000u64;

        // The driver is in the middle of its read-decide-write: it holds the lock and has read an
        // empty claim. The layer claims now; without the lock it would read the empty claim too and
        // then overwrite the driver's.
        let held = driver.lock_claims(CLAIM_LOCK_WAIT).unwrap();
        assert!(layer.lock_claims(std::time::Duration::ZERO).is_none());
        let path2 = path.clone();
        let racer = std::thread::spawn(move || {
            let layer = RigPoseReader::open(&path2).unwrap();
            layer.claim_waiting(ConsumerKind::XR_LAYER, t + 1, std::time::Duration::from_secs(5)).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(layer.consumer().unwrap(), None, "the layer waited");
        driver
            .file
            .write_all_at(&layout::encode_consumer(ConsumerKind::STEAMVR_DRIVER.0, driver.pid, t), CONSUMER_OFFSET)
            .unwrap();
        drop(held);
        assert!(matches!(
            racer.join().unwrap(),
            ClaimOutcome::Yielded(c) if c.kind == ConsumerKind::STEAMVR_DRIVER
        ));
        assert_eq!(layer.consumer().unwrap().map(|c| c.kind), Some(ConsumerKind::STEAMVR_DRIVER));
        // Claims release the lock again.
        assert!(matches!(
            driver.claim(ConsumerKind::STEAMVR_DRIVER, t + 2).unwrap(),
            ClaimOutcome::Granted { recorded: true }
        ));
        assert!(layer.lock_claims(std::time::Duration::ZERO).is_some());
    }

    #[test]
    fn writer_never_touches_the_consumer_region() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        let mut w = RigPoseWriter::create(&path).unwrap();
        let r = RigPoseReader::open(&path).unwrap();
        r.claim(ConsumerKind::STEAMVR_DRIVER, 123).unwrap();
        w.publish(&sample(3)).unwrap();
        drop(w);
        let _w = RigPoseWriter::create(&path).unwrap();
        assert_eq!(r.consumer().unwrap().map(|c| c.heartbeat_ns), Some(123));
    }

    #[test]
    fn claim_decision_table() {
        let now = 5 * CLAIM_TIMEOUT_NS;
        let live = |kind| Some(ConsumerClaim { kind, pid: 1, heartbeat_ns: now });
        let dead = |kind| Some(ConsumerClaim { kind, pid: 1, heartbeat_ns: now - CLAIM_TIMEOUT_NS - 1 });
        assert!(may_claim(None, ConsumerKind::XR_LAYER, now));
        assert!(may_claim(live(ConsumerKind::XR_LAYER), ConsumerKind::XR_LAYER, now));
        assert!(!may_claim(live(ConsumerKind::STEAMVR_DRIVER), ConsumerKind::XR_LAYER, now));
        assert!(may_claim(dead(ConsumerKind::STEAMVR_DRIVER), ConsumerKind::XR_LAYER, now));
        assert!(may_claim(live(ConsumerKind::XR_LAYER), ConsumerKind::STEAMVR_DRIVER, now));
        let future = Some(ConsumerClaim {
            kind: ConsumerKind::STEAMVR_DRIVER,
            pid: 1,
            heartbeat_ns: now + 2 * CLAIM_TIMEOUT_NS,
        });
        assert!(may_claim(future, ConsumerKind::XR_LAYER, now));
    }
}
