//! Epoch-based file lease enforcing one writer per session.
//!
//! Layout: a session file `<id>.jsonl` has a sibling directory
//! `<id>.lease/` holding `lease.json` (fixed metadata: the lease seconds
//! `L`), one `epoch-<n>` marker file per epoch an acquirer has held or
//! attempted (JSON: the host, pid, and RFC 3339 UTC time that opened it),
//! one `heartbeat` file: a 512-byte record (JSON padded with spaces to 511
//! bytes, plus a trailing `\n`) the current holder rewrites periodically,
//! and, once an epoch has been taken over from a holder that had not
//! released it, one `fenced-<n>.jsonl` per taken-over epoch: that epoch's
//! session log, moved aside so the new epoch starts from a log holding only
//! complete records (see [`acquire_with`]). The liveness decision itself
//! compares raw heartbeat bytes across a poll window, so no cross-host
//! clock or process ever has to agree with another on wall-clock time;
//! heartbeat JSON is parsed only to read `released` and to report the
//! current holder's host and pid.
//!
//! Ownership: a [`Lease`] owns an open descriptor on its lease directory
//! for its entire lifetime, so a later rename of `<id>.lease/` does not
//! break renewal or release. Concurrency: [`Lease::acquire`] blocks the
//! calling thread while it polls; once acquired, one process-wide
//! [`Keeper`] renews and watches every held lease on its own threads.
//! `write_lock` serializes heartbeat writes between that renewal thread and
//! an explicit [`Lease::release`] call. Errors: [`AcquireError`] reports
//! why acquisition failed; [`Lease::check`] reports whether the lease has
//! since been lost, for a caller to test before trusting a write durable.

#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use super::children::Children;
use crate::session::fsops;

/// Default lease duration `L`, in seconds, for a newly created lease
/// directory.
pub const DEFAULT_LEASE_SECONDS: u64 = 30;

/// Exit status a fenced process uses when it stops itself: `libc::_exit`
/// with this code, and no further writes.
pub const FENCED_EXIT_STATUS: i32 = 75;

/// Fixed size, in bytes, of the heartbeat record: JSON content, then space
/// padding, then a trailing `\n` at the final byte.
const HEARTBEAT_SIZE: usize = 512;

/// The directory a session's lease lives in: `<id>.jsonl` -> `<id>.lease`.
pub fn lease_dir(session_path: &Path) -> PathBuf {
    session_path.with_extension("lease")
}

/// True when `session_path` has a lease directory. `Err` when the path
/// exists but is not a plain directory (a symlink or a regular file),
/// since neither is a lease directory Otto created.
pub fn is_lease_managed(session_path: &Path) -> std::io::Result<bool> {
    let dir_path = lease_dir(session_path);
    match std::fs::symlink_metadata(&dir_path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(true),
        Ok(meta) if meta.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is a symlink, not a lease directory", dir_path.display()),
        )),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exists but is not a directory", dir_path.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Creates `session_path`'s lease directory with the given `lease_seconds`
/// as `L`, via a temporary directory fsynced and then renamed into place
/// with an exclusive rename, so a concurrent caller doing the same fails
/// instead of silently sharing a half-written directory. When that rename
/// loses the race, the temporary directory is removed and this returns
/// `Ok(())`: the other caller's `lease.json` stays as the lease directory's
/// metadata.
pub fn create_lease_dir(session_path: &Path, lease_seconds: u64) -> std::io::Result<()> {
    if lease_seconds == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "lease_seconds must be greater than zero",
        ));
    }

    let dir_path = lease_dir(session_path);
    let parent = dir_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session path has no parent directory",
        )
    })?;
    let dir_name = dir_path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "lease directory path has no file name",
        )
    })?;

    let tmp_path = parent.join(format!(
        "{}.tmp-{}",
        dir_name.to_string_lossy(),
        crate::auth::random_hex::<8>()?
    ));
    std::fs::create_dir(&tmp_path)?;

    let write_result = (|| {
        let meta = LeaseMeta { lease_seconds };
        let meta_path = tmp_path.join("lease.json");
        std::fs::write(
            &meta_path,
            serde_json::to_vec(&meta).expect("serialize lease meta"),
        )?;
        std::fs::File::open(&meta_path)?.sync_all()?;
        std::fs::File::open(&tmp_path)?.sync_all()
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_dir_all(&tmp_path);
        return Err(e);
    }

    if let Err(e) = fsops::rename_excl(&tmp_path, &dir_path) {
        let _ = std::fs::remove_dir_all(&tmp_path);
        if is_eexist_or_enotempty(&e) {
            return Ok(());
        }
        return Err(e);
    }

    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// The host and pid a lease epoch's heartbeat identifies as its holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub epoch: u64,
    pub host: String,
    pub pid: u32,
}

/// How [`Lease::acquire`] obtained the lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// Epoch 1, or the epoch after a released one; the log was not moved.
    Fresh,
    /// An unreleased holder's epoch, after 7`L`/6 without heartbeat
    /// changes; the log was moved aside.
    TakenOver(Holder),
}

/// Why [`Lease::acquire`] failed.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("session is held by host {} pid {} (lease epoch {})", .0.host, .0.pid, .0.epoch)]
    Held(Holder),
    #[error("another process created lease epoch {0} first")]
    Raced(u64),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LeaseMeta {
    lease_seconds: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HeartbeatRecord {
    epoch: u64,
    host: String,
    pid: u32,
    released: bool,
    seq: u64,
}

fn write_heartbeat_record(dir: &fsops::Dir, record: &HeartbeatRecord) -> std::io::Result<()> {
    let json = serde_json::to_string(record).map_err(std::io::Error::other)?;
    if json.len() > HEARTBEAT_SIZE - 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "heartbeat record too large for its fixed-size slot",
        ));
    }
    let mut bytes = vec![b' '; HEARTBEAT_SIZE];
    bytes[..json.len()].copy_from_slice(json.as_bytes());
    bytes[HEARTBEAT_SIZE - 1] = b'\n';
    let file = fsops::create_at_no_follow(dir, "heartbeat", libc::O_WRONLY, 0o600)?;
    pwrite_all(&file, &bytes, 0)?;
    file.sync_all()?;
    Ok(())
}

fn read_heartbeat_bytes(dir: &fsops::Dir) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fsops::open_at_no_follow(dir, "heartbeat", libc::O_RDONLY)?;
    let mut buf = Vec::with_capacity(HEARTBEAT_SIZE);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn parse_heartbeat(bytes: &[u8]) -> std::io::Result<HeartbeatRecord> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    serde_json::from_str(text.trim_end())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Host and pid to report for a holder whose heartbeat bytes did not parse:
/// some other version's record, or a write caught mid-write. Acquisition
/// never fails on this; it reports an unidentified holder instead.
fn parse_heartbeat_lenient(bytes: &[u8]) -> (String, u32) {
    match parse_heartbeat(bytes) {
        Ok(record) => (record.host, record.pid),
        Err(_) => ("unknown".to_string(), 0),
    }
}

/// Like [`read_heartbeat_bytes`], but a missing heartbeat file reads as
/// empty bytes rather than an error, for callers polling for one to appear.
fn read_heartbeat_bytes_lenient(dir: &fsops::Dir) -> std::io::Result<Vec<u8>> {
    match read_heartbeat_bytes(dir) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The current heartbeat, parsed. `None` covers both a missing heartbeat
/// file and one whose bytes do not parse: [`acquire_with`] treats an
/// unparseable heartbeat the same as an absent one, so a holder's initial
/// record being some other version, or caught mid-write, never turns
/// acquisition into a hard error -- it waits out the liveness window
/// instead.
fn read_heartbeat(dir: &fsops::Dir) -> std::io::Result<Option<HeartbeatRecord>> {
    match read_heartbeat_bytes(dir) {
        Ok(bytes) => Ok(parse_heartbeat(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn pwrite_all(file: &std::fs::File, buf: &[u8], mut offset: i64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let mut written = 0usize;
    while written < buf.len() {
        // SAFETY: fd is open for the duration of the call, and
        // buf[written..] is a valid, initialized slice of the remaining
        // bytes to write.
        let n = unsafe {
            libc::pwrite(
                fd,
                buf[written..].as_ptr() as *const libc::c_void,
                buf.len() - written,
                offset,
            )
        };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        written += n as usize;
        offset += n as i64;
    }
    Ok(())
}

fn local_hostname() -> String {
    let mut buf = vec![0u8; 256];
    // SAFETY: buf has 256 bytes of writable storage; gethostname writes at
    // most that many bytes, including the NUL terminator, into it.
    let result = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if result != 0 {
        return "unknown-host".to_string();
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..nul]).into_owned()
}

fn highest_epoch(dir_path: &Path) -> std::io::Result<u64> {
    let mut highest = 0u64;
    for entry in std::fs::read_dir(dir_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("epoch-") else {
            continue;
        };
        if let Ok(n) = rest.parse::<u64>() {
            highest = highest.max(n);
        }
    }
    Ok(highest)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EpochMarker {
    host: String,
    pid: u32,
    started_at: String,
}

/// Creates `epoch-<epoch>` exclusively and writes its content -- the host
/// and pid opening this epoch, and when, as an RFC 3339 UTC timestamp --
/// fsyncing it before returning, so the marker is durable before the first
/// heartbeat for this epoch is written.
fn create_epoch_marker(dir: &fsops::Dir, epoch: u64) -> std::io::Result<()> {
    use std::io::Write;
    let marker = EpochMarker {
        host: local_hostname(),
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    let json = serde_json::to_vec(&marker).map_err(std::io::Error::other)?;
    let mut file = fsops::create_at_no_follow(
        dir,
        &format!("epoch-{epoch}"),
        libc::O_WRONLY | libc::O_EXCL,
        0o600,
    )?;
    file.write_all(&json)?;
    file.sync_all()
}

/// Reads and validates `lease.json`. A parse error or a stored
/// `lease_seconds` of zero is [`AcquireError::Invalid`], not `Io`: the
/// directory exists and was read fine, its content is what is wrong.
fn read_lease_meta(dir_path: &Path) -> Result<LeaseMeta, AcquireError> {
    let path = dir_path.join("lease.json");
    let text = std::fs::read_to_string(&path).map_err(AcquireError::Io)?;
    let meta: LeaseMeta = serde_json::from_str(&text).map_err(|e| {
        AcquireError::Invalid(format!("{}: invalid lease metadata: {e}", path.display()))
    })?;
    if meta.lease_seconds == 0 {
        return Err(AcquireError::Invalid(format!(
            "{}: lease_seconds must be greater than zero",
            path.display()
        )));
    }
    Ok(meta)
}

fn is_eexist(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AlreadyExists
}

/// True for `EEXIST` or `ENOTEMPTY`: both mean a concurrent
/// [`create_lease_dir`] call won the race to create the lease directory.
fn is_eexist_or_enotempty(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(code) if code == libc::EEXIST || code == libc::ENOTEMPTY)
}

/// Time source [`acquire_with`] polls against while waiting out a possible
/// holder's liveness window. Only acquisition uses this; a held lease's
/// background renewal and watchdog always use the real clock, since they
/// run for the life of the process rather than for one bounded wait.
pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}

struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Duration {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A held session lease: one epoch, on one host, for one process.
#[derive(Debug)]
pub struct Lease {
    epoch: u64,
    host: String,
    pid: u32,
    duration_secs: u64,
    dir: fsops::Dir,
    write_lock: Mutex<()>,
    last_renewal: Mutex<Option<Instant>>,
    seq: AtomicU64,
    fenced: AtomicBool,
    superseded: AtomicBool,
    released: AtomicBool,
    lost_reason: Mutex<Option<String>>,
    /// Test-only failure injection: when set, the next heartbeat write
    /// reports an error instead of writing, so a test can force renewal to
    /// fail deterministically rather than racing the watchdog's tick
    /// against a renewal thread that keeps succeeding.
    fail_renewal: AtomicBool,
}

impl Lease {
    /// Acquires the lease for `session_path`'s lease directory (see
    /// [`create_lease_dir`]), waiting out a running holder's liveness
    /// window if needed. Registers the lease with the process-wide
    /// [`Keeper`], which renews it every `L`/3 and fences this process if
    /// renewal falls behind or a successor epoch appears.
    pub fn acquire(session_path: &Path) -> Result<(Arc<Lease>, Acquired), AcquireError> {
        acquire_with(session_path, &RealClock, Keeper::global())
    }

    /// The epoch this lease holds.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The lease's configured duration `L`.
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.duration_secs)
    }

    /// `Err` once this process has been fenced (its watchdog decided
    /// renewal fell behind, or a successor epoch appeared), with text
    /// starting "session lease lost". A caller uses this to refuse to
    /// treat a tool call's effects as durable once the lease backing the
    /// session may have moved to another host.
    ///
    /// Evaluates the same two conditions the watchdog fences on --
    /// [`Lease::should_fence`] -- itself, rather than only trusting the
    /// `fenced` flag the watchdog sets: a caller between renewal thread
    /// ticks must see a lease it can no longer trust before that thread
    /// gets to it.
    pub fn check(&self) -> Result<(), String> {
        if self.fenced.load(Ordering::Acquire) {
            let reason = self.lost_reason.lock().unwrap().clone().unwrap_or_default();
            return Err(format!("session lease lost: {reason}"));
        }
        if let Some(reason) = self.should_fence() {
            return Err(format!("session lease lost: {reason}"));
        }
        Ok(())
    }

    /// Marks the lease released: the next `acquire` on this directory
    /// takes over immediately, without waiting out a liveness window.
    /// Safe to call after the lease directory has been renamed, since
    /// writes go through the descriptor opened at acquisition time.
    ///
    /// A no-op once this lease is lost -- a successor epoch appeared
    /// (`superseded`) or the watchdog already fenced this process
    /// (`fenced`) -- rather than an error: a successor epoch's acquirer now
    /// owns the heartbeat file, so a released record written from this
    /// epoch would overwrite the successor's own record. Returns `Ok(())`
    /// without writing and without setting `released`.
    pub fn release(&self) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        if self.superseded.load(Ordering::Acquire) || self.fenced.load(Ordering::Acquire) {
            return Ok(());
        }
        self.released.store(true, Ordering::Release);
        self.write_heartbeat_locked(true)
    }

    fn write_heartbeat(&self, released: bool) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        self.write_heartbeat_locked(released)
    }

    fn write_heartbeat_locked(&self, released: bool) -> std::io::Result<()> {
        if self.fail_renewal.load(Ordering::Acquire) {
            return Err(std::io::Error::other("injected renewal failure (test)"));
        }
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let record = HeartbeatRecord {
            epoch: self.epoch,
            host: self.host.clone(),
            pid: self.pid,
            released,
            seq,
        };
        write_heartbeat_record(&self.dir, &record)
    }

    fn set_lost_reason(&self, reason: String) {
        *self.lost_reason.lock().unwrap() = Some(reason);
    }

    /// Checks whether a successor epoch has appeared and, only if not,
    /// renews the heartbeat if `L`/3 has passed since the last successful
    /// renewal. Called by the `Keeper`'s renewal thread.
    ///
    /// The successor check runs first, in the same tick, so a lease found
    /// superseded here writes no further heartbeat record: a successor
    /// epoch's acquirer already owns the heartbeat file at that point, and
    /// one more record from this epoch would overwrite the successor's own.
    fn maybe_renew(&self) {
        if self.released.load(Ordering::Acquire) || self.fenced.load(Ordering::Acquire) {
            return;
        }
        if let Ok(true) =
            fsops::exists_at_no_follow(&self.dir, &format!("epoch-{}", self.epoch + 1))
        {
            self.superseded.store(true, Ordering::Release);
            return;
        }
        let last = *self.last_renewal.lock().unwrap();
        let due = last.is_none_or(|instant| instant.elapsed() >= self.duration() / 3);
        if due {
            let reading = Instant::now();
            let _guard = self.write_lock.lock().unwrap();
            if !self.released.load(Ordering::Acquire)
                && !self.superseded.load(Ordering::Acquire)
                && self.write_heartbeat_locked(false).is_ok()
            {
                *self.last_renewal.lock().unwrap() = Some(reading);
            }
        }
    }

    /// `Some(reason)` when the `Keeper`'s watchdog should fence this
    /// process: renewal has not succeeded in over 5`L`/6, or a successor
    /// epoch has appeared.
    fn should_fence(&self) -> Option<String> {
        if self.released.load(Ordering::Acquire) {
            return None;
        }
        if self.superseded.load(Ordering::Acquire) {
            return Some(format!(
                "a successor epoch was created while this process held epoch {}",
                self.epoch
            ));
        }
        let last = *self.last_renewal.lock().unwrap();
        match last {
            Some(instant) if instant.elapsed() > self.duration() * 5 / 6 => Some(format!(
                "no successful heartbeat renewal for {:?}, over 5/6 of the {:?} lease",
                instant.elapsed(),
                self.duration()
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
impl Lease {
    pub(crate) fn for_test(dir_path: &Path) -> Arc<Lease> {
        let dir = fsops::open_dir_no_follow(dir_path).expect("open test lease dir");
        let lease = Arc::new(Lease {
            epoch: 1,
            host: "test-host".into(),
            pid: std::process::id(),
            duration_secs: DEFAULT_LEASE_SECONDS,
            dir,
            write_lock: Mutex::new(()),
            last_renewal: Mutex::new(Some(Instant::now())),
            seq: AtomicU64::new(0),
            fenced: AtomicBool::new(false),
            superseded: AtomicBool::new(false),
            released: AtomicBool::new(false),
            lost_reason: Mutex::new(None),
            fail_renewal: AtomicBool::new(false),
        });
        lease
            .write_heartbeat(false)
            .expect("write initial heartbeat");
        lease
    }

    pub(crate) fn mark_lost_for_test(&self, reason: &str) {
        self.fenced.store(true, Ordering::Release);
        self.set_lost_reason(reason.to_string());
    }

    /// Backdates the last successful renewal, so [`Lease::check`] and
    /// [`Lease::should_fence`] can be tested against a stale renewal
    /// without a `Keeper`'s background threads running at all.
    pub(crate) fn set_last_renewal_for_test(&self, when: Instant) {
        *self.last_renewal.lock().unwrap() = Some(when);
    }

    /// Makes the next heartbeat write -- renewal or release -- report an
    /// error instead of writing, for deterministically testing what
    /// happens when renewal cannot succeed.
    pub(crate) fn fail_renewal_for_test(&self) {
        self.fail_renewal.store(true, Ordering::Release);
    }
}

/// Renews and watches every held lease in this process. One process-wide
/// instance ([`Keeper::global`]) drives real fencing (`libc::_exit`); tests
/// build their own with [`Keeper::new_for_test`] and a non-exiting action,
/// so background threads never terminate the test process. A test
/// `Keeper`'s threads hold only a `Weak` reference to it and exit once its
/// last `Arc` drops.
///
/// `children` is the registry `fence` kills through. [`Keeper::global`] uses
/// [`Children::global`]; a test `Keeper` gets its own isolated instance, so
/// its `kill_all` cannot reach a process group a concurrently running test
/// registered on the global registry.
pub(crate) struct Keeper {
    state: Mutex<Vec<Weak<Lease>>>,
    threads_started: Mutex<bool>,
    on_fence: Box<dyn Fn() + Send + Sync>,
    children: Arc<Children>,
    /// How often the renewal and watchdog threads wake to check their
    /// leases. Production leaves this at one second; a test keeper can
    /// shorten it so a small `L` does not have to wait out a full
    /// production tick to see renewal or fencing happen.
    tick_interval: Duration,
}

impl Keeper {
    fn new(
        on_fence: Box<dyn Fn() + Send + Sync>,
        children: Arc<Children>,
        tick_interval: Duration,
    ) -> Arc<Keeper> {
        Arc::new(Keeper {
            state: Mutex::new(Vec::new()),
            threads_started: Mutex::new(false),
            on_fence,
            children,
            tick_interval,
        })
    }

    /// The process-wide instance. Fences this process for real: sends
    /// `SIGKILL` to every child started by [`Children::global`], then
    /// `libc::_exit(FENCED_EXIT_STATUS)`.
    pub(crate) fn global() -> &'static Arc<Keeper> {
        static INSTANCE: OnceLock<Arc<Keeper>> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            Keeper::new(
                Box::new(|| {
                    // SAFETY: _exit is always valid to call and does not run
                    // destructors, matching the "exits without writing"
                    // fence contract.
                    unsafe { libc::_exit(FENCED_EXIT_STATUS) }
                }),
                Arc::clone(Children::global()),
                Duration::from_secs(1),
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        on_fence: Box<dyn Fn() + Send + Sync>,
        children: Arc<Children>,
        tick_interval: Duration,
    ) -> Arc<Keeper> {
        Keeper::new(on_fence, children, tick_interval)
    }

    fn register(keeper: &Arc<Keeper>, lease: &Arc<Lease>) {
        keeper.state.lock().unwrap().push(Arc::downgrade(lease));
        Keeper::ensure_threads(keeper);
    }

    fn ensure_threads(keeper: &Arc<Keeper>) {
        let mut started = keeper.threads_started.lock().unwrap();
        if *started {
            return;
        }
        *started = true;
        let renewal = Arc::downgrade(keeper);
        std::thread::Builder::new()
            .name("otto-lease-renewal".into())
            .spawn(move || Keeper::renewal_loop(renewal))
            .expect("spawn lease renewal thread");
        let watchdog = Arc::downgrade(keeper);
        std::thread::Builder::new()
            .name("otto-lease-watchdog".into())
            .spawn(move || Keeper::watchdog_loop(watchdog))
            .expect("spawn lease watchdog thread");
    }

    fn renewal_loop(keeper: Weak<Keeper>) {
        loop {
            let Some(interval) = keeper.upgrade().map(|k| k.tick_interval) else {
                return;
            };
            std::thread::sleep(interval);
            let Some(keeper) = keeper.upgrade() else {
                return;
            };
            for lease in keeper.live_leases() {
                lease.maybe_renew();
            }
        }
    }

    fn watchdog_loop(keeper: Weak<Keeper>) {
        loop {
            let Some(interval) = keeper.upgrade().map(|k| k.tick_interval) else {
                return;
            };
            std::thread::sleep(interval);
            let Some(keeper) = keeper.upgrade() else {
                return;
            };
            for lease in keeper.live_leases() {
                if let Some(reason) = lease.should_fence() {
                    keeper.fence(&lease, reason);
                }
            }
        }
    }

    fn live_leases(&self) -> Vec<Arc<Lease>> {
        let mut state = self.state.lock().unwrap();
        let mut live = Vec::new();
        state.retain(|weak| match weak.upgrade() {
            Some(lease) => {
                live.push(lease);
                true
            }
            None => false,
        });
        live
    }

    fn fence(&self, lease: &Arc<Lease>, reason: String) {
        if lease.fenced.swap(true, Ordering::AcqRel) {
            return;
        }
        lease.set_lost_reason(reason);
        self.children.kill_all();
        (self.on_fence)();
    }
}

/// Acquires `session_path`'s lease, using `clock` for the liveness-window
/// wait and `keeper` to register the lease once held. Implements the
/// protocol: with no epoch file, create epoch 1; otherwise read the
/// heartbeat once. A missing heartbeat waits, since it may belong to a
/// concurrent acquirer that has not written its first heartbeat yet. A
/// released heartbeat is taken over immediately only when its epoch is the
/// highest epoch marker present; a released heartbeat behind a newer marker
/// waits, for the same reason. Otherwise poll the heartbeat's raw bytes once
/// a second for 7`L`/6 — any change means the holder is live and acquisition
/// fails, no change for the whole window means it takes over. `EEXIST` on
/// the epoch create means a concurrent acquirer won the race.
pub(crate) fn acquire_with(
    session_path: &Path,
    clock: &dyn Clock,
    keeper: &Arc<Keeper>,
) -> Result<(Arc<Lease>, Acquired), AcquireError> {
    let dir_path = lease_dir(session_path);
    let dir = fsops::open_dir_no_follow(&dir_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AcquireError::Invalid(format!(
                "{} does not exist; call create_lease_dir first",
                dir_path.display()
            ))
        } else {
            AcquireError::Io(e)
        }
    })?;

    let meta = read_lease_meta(&dir_path)?;
    let l = Duration::from_secs(meta.lease_seconds);
    let highest = highest_epoch(&dir_path)?;

    if highest == 0 {
        return match create_epoch_marker(&dir, 1) {
            Ok(()) => finish_acquire(dir, 1, meta.lease_seconds, keeper, Acquired::Fresh),
            Err(e) if is_eexist(&e) => Err(AcquireError::Raced(1)),
            Err(e) => Err(AcquireError::Io(e)),
        };
    }

    let heartbeat = read_heartbeat(&dir)?;
    // A missing heartbeat waits out the same liveness window as a live one,
    // rather than being treated as an immediate takeover: the epoch marker
    // this process just saw may belong to a concurrent acquirer that has
    // not yet written its first heartbeat (`finish_acquire` writes the
    // marker, then the heartbeat, as two separate steps). Waiting gives
    // that heartbeat a chance to appear; only a marker whose heartbeat
    // never appears within the window is treated as an abandoned epoch.
    //
    // A released heartbeat is only trusted when it belongs to the highest
    // epoch marker: a released heartbeat behind a newer marker means a
    // concurrent acquirer already created that marker but has not written
    // its heartbeat yet, so waiting is needed here too, or two acquirers
    // would both take over and append to the log concurrently.
    let should_wait = !matches!(&heartbeat, Some(h) if h.released && h.epoch == highest);

    let mut prior_holder = heartbeat.as_ref().map(|h| Holder {
        epoch: highest,
        host: h.host.clone(),
        pid: h.pid,
    });

    if should_wait {
        let initial = read_heartbeat_bytes_lenient(&dir)?;
        let deadline = clock.now() + (l * 7 / 6);
        loop {
            if clock.now() >= deadline {
                break;
            }
            clock.sleep(Duration::from_secs(1));
            let current = read_heartbeat_bytes_lenient(&dir)?;
            if current != initial {
                if current.is_empty() {
                    // The heartbeat that appeared at `initial` was removed;
                    // nothing more to compare against.
                    continue;
                }
                let (host, pid) = parse_heartbeat_lenient(&current);
                return Err(AcquireError::Held(Holder {
                    epoch: highest,
                    host,
                    pid,
                }));
            }
        }
        // The window elapsed with no observed change. A heartbeat that
        // appeared during the wait (from a concurrent acquirer finishing
        // its own acquisition) but then stayed identical belongs to the
        // holder being taken over now.
        if !initial.is_empty() {
            let (host, pid) = parse_heartbeat_lenient(&initial);
            prior_holder = Some(Holder {
                epoch: highest,
                host,
                pid,
            });
        }
    }

    let next = highest + 1;
    match create_epoch_marker(&dir, next) {
        Ok(()) if should_wait => {
            // Taking over an unreleased epoch: the prior holder may still
            // hold a descriptor open on the session log, so its tail may
            // hold an incomplete record. Move it aside before any new write
            // reaches the log under this epoch.
            let holder = prior_holder.unwrap_or(Holder {
                epoch: highest,
                host: "unknown".into(),
                pid: 0,
            });
            if let Err(e) = move_aside_log(session_path, &dir_path, highest) {
                return Err(AcquireError::Io(e));
            }
            finish_acquire(
                dir,
                next,
                meta.lease_seconds,
                keeper,
                Acquired::TakenOver(holder),
            )
        }
        Ok(()) => {
            // The prior holder reported itself released; its log is already
            // complete, so there is nothing to move aside.
            finish_acquire(dir, next, meta.lease_seconds, keeper, Acquired::Fresh)
        }
        Err(e) if is_eexist(&e) => Err(AcquireError::Raced(next)),
        Err(e) => Err(AcquireError::Io(e)),
    }
}

/// Finds the highest-numbered `fenced-<n>.jsonl` in `dir_path`, or `None` if
/// none exists.
fn highest_fenced_path(dir_path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut highest: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("fenced-") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(".jsonl") else {
            continue;
        };
        if let Ok(n) = rest.parse::<u64>()
            && highest.as_ref().is_none_or(|(h, _)| n > *h)
        {
            highest = Some((n, entry.path()));
        }
    }
    Ok(highest.map(|(_, path)| path))
}

/// Moves `session_path`'s log aside when taking over epoch `epoch` from a
/// holder that had not released it, so the new epoch starts from a log
/// holding only complete records: renames the old log into the lease
/// directory as `fenced-<epoch>.jsonl`, then writes a copy of it, truncated
/// to its last complete line, back to `session_path` via a temp file and
/// rename. Rejects a source over
/// [`otto_core::session::MAX_SESSION_FILE_BYTES`] without reading it, so an
/// oversized log is left untouched rather than silently truncated. When
/// `session_path` is already missing -- because an earlier attempt got as
/// far as the first rename before being interrupted, possibly at a
/// different epoch than the one being retried now -- the highest-numbered
/// `fenced-*.jsonl` present is reused as the source instead of failing, so a
/// retried acquire finishes the same move; only when no `fenced-*.jsonl`
/// exists at all does this return `Ok` with nothing done. Fsyncs both the
/// lease directory and `session_path`'s parent directory before returning,
/// so both directories' entry changes are durable.
fn move_aside_log(session_path: &Path, dir_path: &Path, epoch: u64) -> std::io::Result<()> {
    let fenced_path = dir_path.join(format!("fenced-{epoch}.jsonl"));

    let source_path = match std::fs::rename(session_path, &fenced_path) {
        Ok(()) => fenced_path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && fenced_path.exists() => {
            // A previous attempt already moved the log aside at this epoch.
            fenced_path
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A previous attempt may have moved the log aside under a
            // different epoch before crashing. Fall back to the highest
            // fenced copy present; if none exists, there was never a log.
            match highest_fenced_path(dir_path)? {
                Some(path) => path,
                None => return Ok(()),
            }
        }
        Err(e) => return Err(e),
    };

    let len = std::fs::metadata(&source_path)?.len();
    let max = otto_core::session::MAX_SESSION_FILE_BYTES as u64;
    if len > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} is {len} bytes, over the {max}-byte session file limit",
                source_path.display()
            ),
        ));
    }

    let mut bytes = std::fs::read(&source_path)?;
    let complete_len = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    bytes.truncate(complete_len);

    let suffix = crate::auth::random_hex::<8>()?;
    let temp_path = dir_path.join(format!("restore.tmp-{suffix}"));
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // 0600, matching the mode `Store` creates session logs with.
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;
        std::fs::rename(&temp_path, session_path)?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    std::fs::File::open(dir_path)?.sync_all()?;
    if let Some(parent) = session_path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }

    Ok(())
}

fn finish_acquire(
    dir: fsops::Dir,
    epoch: u64,
    lease_seconds: u64,
    keeper: &Arc<Keeper>,
    acquired: Acquired,
) -> Result<(Arc<Lease>, Acquired), AcquireError> {
    let lease = Arc::new(Lease {
        epoch,
        host: local_hostname(),
        pid: std::process::id(),
        duration_secs: lease_seconds,
        dir,
        write_lock: Mutex::new(()),
        last_renewal: Mutex::new(Some(Instant::now())),
        seq: AtomicU64::new(0),
        fenced: AtomicBool::new(false),
        superseded: AtomicBool::new(false),
        released: AtomicBool::new(false),
        lost_reason: Mutex::new(None),
        fail_renewal: AtomicBool::new(false),
    });
    lease.write_heartbeat(false).map_err(AcquireError::Io)?;
    Keeper::register(keeper, &lease);
    Ok((lease, acquired))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::atomic::AtomicU32;

    struct FakeClock {
        now: Mutex<Duration>,
        sleeps: Mutex<u32>,
        on_sleep: Box<dyn Fn() + Send + Sync>,
    }

    impl FakeClock {
        fn new() -> Self {
            FakeClock::with_on_sleep(|| {})
        }

        fn with_on_sleep(on_sleep: impl Fn() + Send + Sync + 'static) -> Self {
            FakeClock {
                now: Mutex::new(Duration::ZERO),
                sleeps: Mutex::new(0),
                on_sleep: Box::new(on_sleep),
            }
        }

        fn sleep_count(&self) -> u32 {
            *self.sleeps.lock().unwrap()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, duration: Duration) {
            (self.on_sleep)();
            *self.sleeps.lock().unwrap() += 1;
            *self.now.lock().unwrap() += duration;
        }
    }

    /// The tick interval test keepers use: fast enough that a small `L`
    /// does not have to wait out a full 1s production tick to see renewal
    /// or fencing happen, so watchdog tests finish well under 2s.
    const TEST_TICK: Duration = Duration::from_millis(50);

    fn no_op_keeper() -> Arc<Keeper> {
        Keeper::new_for_test(Box::new(|| {}), Arc::new(Children::new()), TEST_TICK)
    }

    #[test]
    fn acquire_with_creates_epoch_1_when_none_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();

        let clock = FakeClock::new();
        let keeper = no_op_keeper();
        let (lease, acquired) = acquire_with(&session_path, &clock, &keeper).unwrap();

        assert_eq!(acquired, Acquired::Fresh);
        assert_eq!(lease.epoch(), 1);
        assert_eq!(clock.sleep_count(), 0, "fresh acquire does not wait");
    }

    #[test]
    fn acquire_with_reports_a_running_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();

        let keeper1 = no_op_keeper();
        let (_lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();

        let dir_path = lease_dir(&session_path);
        let renew_dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        let seq = AtomicU32::new(1);
        let clock2 = FakeClock::with_on_sleep(move || {
            let record = HeartbeatRecord {
                epoch: 1,
                host: "holder-host".into(),
                pid: 4242,
                released: false,
                seq: seq.fetch_add(1, Ordering::Relaxed) as u64,
            };
            write_heartbeat_record(&renew_dir, &record).unwrap();
        });
        let keeper2 = no_op_keeper();
        let err = acquire_with(&session_path, &clock2, &keeper2).unwrap_err();

        match err {
            AcquireError::Held(holder) => {
                assert_eq!(holder.epoch, 1);
                assert_eq!(holder.host, "holder-host");
                assert_eq!(holder.pid, 4242);
            }
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn acquire_with_takes_over_a_stopped_holder_after_the_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

        let keeper1 = no_op_keeper();
        let (_lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();

        let clock2 = FakeClock::new();
        let keeper2 = no_op_keeper();
        let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

        assert_eq!(lease2.epoch(), 2);
        match acquired {
            Acquired::TakenOver(holder) => assert_eq!(holder.epoch, 1),
            other => panic!("expected TakenOver, got {other:?}"),
        }
        assert!(clock2.sleep_count() >= 7, "expected to wait out 7L/6");
    }

    #[test]
    fn acquire_with_takes_over_a_released_holder_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();
        std::fs::write(&session_path, b"{\"line\":1}\n").unwrap();

        use std::os::unix::fs::MetadataExt;
        let inode_before = std::fs::metadata(&session_path).unwrap().ino();

        let keeper1 = no_op_keeper();
        let (lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();
        lease1.release().unwrap();

        let clock2 = FakeClock::new();
        let keeper2 = no_op_keeper();
        let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

        assert_eq!(lease2.epoch(), 2);
        assert_eq!(
            acquired,
            Acquired::Fresh,
            "a released holder's epoch is taken over as Fresh, not TakenOver"
        );
        assert_eq!(
            clock2.sleep_count(),
            0,
            "a released holder is taken over without waiting"
        );
        assert_eq!(
            std::fs::metadata(&session_path).unwrap().ino(),
            inode_before,
            "the log keeps its inode when taking over a released epoch"
        );
        assert_eq!(
            std::fs::read(&session_path).unwrap(),
            b"{\"line\":1}\n",
            "the log is not moved aside when taking over a released epoch"
        );
        let dir_path = lease_dir(&session_path);
        assert!(
            !dir_path.join("fenced-1.jsonl").exists(),
            "no fenced copy is created for a released epoch"
        );
    }

    #[test]
    fn concurrent_acquire_with_races_exactly_one_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();
        let session_path = Arc::new(session_path);

        let mut handles = Vec::new();
        for _ in 0..4 {
            let session_path = Arc::clone(&session_path);
            handles.push(std::thread::spawn(move || {
                // A losing thread's wait loop advances a `FakeClock`
                // instantly; without a real delay on each tick it could
                // finish waiting out the liveness window before the
                // winning thread's `finish_acquire` (real syscalls) gets a
                // scheduler slice to write its heartbeat, and wrongly treat
                // the winner's epoch as abandoned.
                let clock = FakeClock::with_on_sleep(|| {
                    std::thread::sleep(Duration::from_millis(5));
                });
                acquire_with(&session_path, &clock, no_op_keeper2())
            }));
        }
        fn no_op_keeper2() -> &'static Arc<Keeper> {
            // a fresh test keeper per call would race registering threads
            // pointlessly; a leaked static is fine for this one test.
            static KEEPER: OnceLock<Arc<Keeper>> = OnceLock::new();
            KEEPER.get_or_init(|| {
                Keeper::new_for_test(Box::new(|| {}), Arc::new(Children::new()), TEST_TICK)
            })
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();
        // A losing thread reports either `Raced(1)`, if it lost the
        // exclusive-create race for the epoch-1 marker outright, or
        // `Held`, if it observed the marker already existing and then
        // waited out the liveness window and saw the winner's heartbeat: a
        // thread that arrives after the marker exists must not treat the
        // still-forming epoch as abandoned and race a takeover against it.
        let lost = results
            .iter()
            .filter(|r| {
                matches!(r, Err(AcquireError::Raced(1)))
                    || matches!(r, Err(AcquireError::Held(h)) if h.epoch == 1)
            })
            .count();
        assert_eq!(successes, 1, "exactly one acquirer should win epoch 1");
        assert_eq!(lost, 3, "the rest should observe the race or the winner");
    }

    #[test]
    fn concurrent_create_lease_dir_has_exactly_one_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = Arc::new(tmp.path().join("s.jsonl"));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let session_path = Arc::clone(&session_path);
            handles.push(std::thread::spawn(move || {
                create_lease_dir(&session_path, 6)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Every caller observes the lease directory in place by the time it
        // returns, win or lose: a loser removes its own temp directory and
        // reports success rather than an error.
        assert!(results.iter().all(|r| r.is_ok()), "{results:?}");

        let dir_path = lease_dir(&session_path);
        let lease_json_count = std::fs::read_dir(&dir_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() == "lease.json")
            .count();
        assert_eq!(lease_json_count, 1, "exactly one lease.json must exist");

        let leftover_tmp_dirs = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftover_tmp_dirs, 0, "no *.tmp-* directories may remain");
    }

    #[test]
    fn heartbeat_reads_are_never_torn_under_concurrent_renewal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().to_path_buf();
        let writer_dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        let record = HeartbeatRecord {
            epoch: 1,
            host: "h".into(),
            pid: 1,
            released: false,
            seq: 0,
        };
        write_heartbeat_record(&writer_dir, &record).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let mut seq = 1u64;
            while !writer_stop.load(Ordering::Relaxed) {
                let record = HeartbeatRecord {
                    epoch: 1,
                    host: "h".into(),
                    pid: 1,
                    released: false,
                    seq,
                };
                write_heartbeat_record(&writer_dir, &record).unwrap();
                seq += 1;
            }
        });

        let reader_dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        for _ in 0..2000 {
            let bytes = read_heartbeat_bytes(&reader_dir).unwrap();
            assert_eq!(bytes.len(), HEARTBEAT_SIZE);
            assert_eq!(bytes[HEARTBEAT_SIZE - 1], b'\n');
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn independent_directory_opens_see_each_others_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().to_path_buf();

        let lease_a = Lease::for_test(&dir_path);
        let lease_b = Lease::for_test(&dir_path);

        lease_b.write_heartbeat(false).unwrap();

        let bytes = read_heartbeat_bytes(&lease_a.dir).unwrap();
        let record = parse_heartbeat(&bytes).unwrap();
        assert_eq!(record.host, "test-host");
        assert!(record.seq >= 1);
    }

    #[test]
    fn watchdog_fences_and_kills_a_real_child_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 1).unwrap(); // L=1s, 5L/6 ~= 0.83s

        let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();
        // Renewal must never succeed, so fencing cannot race a renewal
        // thread that keeps the lease alive: without this, the test only
        // passes because the renewal and watchdog threads' first ticks
        // happen to race in the watchdog's favor.
        lease.fail_renewal_for_test();

        // A private registry: kill_all must never reach a process group a
        // concurrently running sandbox::process test registered on the
        // global one.
        let children = Arc::new(Children::new());
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let _registration = Children::register(&children, pid, true);

        let fenced = Arc::new(AtomicBool::new(false));
        let fenced_flag = Arc::clone(&fenced);
        let keeper = Keeper::new_for_test(
            Box::new(move || fenced_flag.store(true, Ordering::Release)),
            Arc::clone(&children),
            TEST_TICK,
        );
        Keeper::register(&keeper, &lease);

        let deadline = Instant::now() + Duration::from_millis(1900);
        loop {
            if fenced.load(Ordering::Acquire) {
                break;
            }
            assert!(Instant::now() < deadline, "watchdog did not fence in time");
            std::thread::sleep(Duration::from_millis(10));
        }

        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            assert!(Instant::now() < deadline, "child process was not killed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lease.check().is_err());
    }

    #[test]
    fn watchdog_fences_when_a_successor_epoch_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 1).unwrap();

        let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

        let dir_path = lease_dir(&session_path);
        let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        create_epoch_marker(&dir, 2).unwrap();

        let children = Arc::new(Children::new());
        let fenced = Arc::new(AtomicBool::new(false));
        let fenced_flag = Arc::clone(&fenced);
        let keeper = Keeper::new_for_test(
            Box::new(move || fenced_flag.store(true, Ordering::Release)),
            children,
            TEST_TICK,
        );
        Keeper::register(&keeper, &lease);

        let deadline = Instant::now() + Duration::from_millis(1900);
        loop {
            if fenced.load(Ordering::Acquire) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "watchdog did not react to a successor epoch"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lease.check().unwrap_err().starts_with("session lease lost"));
    }

    #[test]
    fn release_after_rename_still_updates_the_heartbeat_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();

        let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

        let old_dir = lease_dir(&session_path);
        let new_dir = tmp.path().join("archived.lease");
        std::fs::rename(&old_dir, &new_dir).unwrap();

        lease.release().unwrap();

        let reopened = fsops::open_dir_no_follow(&new_dir).unwrap();
        let bytes = read_heartbeat_bytes(&reopened).unwrap();
        let record = parse_heartbeat(&bytes).unwrap();
        assert!(record.released);
    }

    #[test]
    fn release_does_not_write_once_superseded() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();

        let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

        let dir_path = lease_dir(&session_path);
        let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        create_epoch_marker(&dir, 2).unwrap();

        // One renewal tick observes the successor epoch and marks this
        // lease superseded, without writing another heartbeat record.
        lease.maybe_renew();
        assert!(
            lease.check().unwrap_err().contains("successor epoch"),
            "expected the successor-epoch check to mark this lease superseded"
        );

        let before = read_heartbeat_bytes(&dir).unwrap();
        lease.release().unwrap();
        let after = read_heartbeat_bytes(&dir).unwrap();

        assert_eq!(after, before, "release must not write once superseded");
        let record = parse_heartbeat(&after).unwrap();
        assert!(
            !record.released,
            "release is a no-op once superseded, so the record stays unreleased"
        );
    }

    #[test]
    fn maybe_renew_does_not_write_once_a_successor_epoch_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();

        let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

        let dir_path = lease_dir(&session_path);
        let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        let before = read_heartbeat_bytes(&dir).unwrap();

        create_epoch_marker(&dir, 2).unwrap();
        // Force the tick to consider renewal due, so the test exercises the
        // successor check racing an otherwise-due renewal, not just a tick
        // that would have skipped writing anyway.
        lease.set_last_renewal_for_test(Instant::now() - lease.duration());

        lease.maybe_renew();

        let after = read_heartbeat_bytes(&dir).unwrap();
        assert_eq!(
            after, before,
            "a renewal tick after a successor epoch appears must not write another record"
        );
    }

    #[test]
    fn check_fails_when_the_last_renewal_is_older_than_5l_over_6() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = Lease::for_test(tmp.path());
        // DEFAULT_LEASE_SECONDS is 30s; 5L/6 = 25s. No Keeper is ever
        // registered, so this exercises `check()`'s own evaluation, not the
        // watchdog thread.
        lease.set_last_renewal_for_test(Instant::now() - Duration::from_secs(26));

        let err = lease.check().unwrap_err();
        assert!(err.starts_with("session lease lost"), "got: {err}");
    }

    #[test]
    fn acquire_with_takes_over_an_unparseable_heartbeat_after_the_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

        let dir_path = lease_dir(&session_path);
        let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        create_epoch_marker(&dir, 1).unwrap();
        // Fixed-size garbage bytes, not valid JSON: acquisition must never
        // hard-fail on this, and identical garbage on every poll must read
        // as "unchanged" rather than "different holder each time".
        let mut garbage = vec![b'x'; HEARTBEAT_SIZE];
        garbage[HEARTBEAT_SIZE - 1] = b'\n';
        let heartbeat_file =
            fsops::create_at_no_follow(&dir, "heartbeat", libc::O_WRONLY, 0o600).unwrap();
        pwrite_all(&heartbeat_file, &garbage, 0).unwrap();
        heartbeat_file.sync_all().unwrap();

        let clock = FakeClock::new();
        let keeper = no_op_keeper();
        let (lease, acquired) = acquire_with(&session_path, &clock, &keeper).unwrap();

        assert_eq!(lease.epoch(), 2);
        match acquired {
            Acquired::TakenOver(holder) => {
                assert_eq!(holder.host, "unknown");
                assert_eq!(holder.pid, 0);
            }
            other => panic!("expected TakenOver, got {other:?}"),
        }
        assert!(clock.sleep_count() >= 7, "expected to wait out 7L/6");
    }

    #[test]
    fn move_aside_log_keeps_only_complete_lines_and_fences_the_old_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();
        let dir_path = lease_dir(&session_path);
        std::fs::write(&session_path, b"{\"a\":1}\n{\"a\":2}\n{\"a\":3, incomplete").unwrap();

        move_aside_log(&session_path, &dir_path, 1).unwrap();

        let fenced = std::fs::read(dir_path.join("fenced-1.jsonl")).unwrap();
        assert_eq!(fenced, b"{\"a\":1}\n{\"a\":2}\n{\"a\":3, incomplete");

        let new_log = std::fs::read(&session_path).unwrap();
        assert_eq!(new_log, b"{\"a\":1}\n{\"a\":2}\n");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&session_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the restored log is owner-only");
    }

    #[test]
    fn move_aside_log_does_not_affect_a_stale_writer_descriptor_to_the_old_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();
        let dir_path = lease_dir(&session_path);
        std::fs::write(&session_path, b"{\"a\":1}\n").unwrap();

        // A writer holding the old path open before the takeover.
        use std::io::Write;
        let mut stale = std::fs::OpenOptions::new()
            .append(true)
            .open(&session_path)
            .unwrap();

        move_aside_log(&session_path, &dir_path, 1).unwrap();

        // The stale descriptor still refers to the renamed (fenced) inode,
        // not the new file at `session_path`.
        stale.write_all(b"{\"a\":2}\n").unwrap();
        stale.sync_all().unwrap();

        assert_eq!(std::fs::read(&session_path).unwrap(), b"{\"a\":1}\n");
        assert_eq!(
            std::fs::read(dir_path.join("fenced-1.jsonl")).unwrap(),
            b"{\"a\":1}\n{\"a\":2}\n"
        );
    }

    #[test]
    fn move_aside_log_restores_from_an_existing_fenced_copy_when_the_log_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();
        let dir_path = lease_dir(&session_path);
        // Simulates a crash after the first rename of an earlier,
        // interrupted attempt: the fenced copy exists, `session_path` does
        // not.
        std::fs::write(dir_path.join("fenced-1.jsonl"), b"{\"a\":1}\n").unwrap();

        move_aside_log(&session_path, &dir_path, 1).unwrap();

        assert_eq!(std::fs::read(&session_path).unwrap(), b"{\"a\":1}\n");
    }

    #[test]
    fn move_aside_log_restores_from_the_highest_fenced_copy_at_a_different_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();
        let dir_path = lease_dir(&session_path);
        // Simulates: acquirer X created epoch 2, renamed `s.jsonl` to
        // `fenced-1.jsonl` (fencing epoch 1, the epoch it took over), then
        // crashed before restoring `s.jsonl`. Acquirer Y now retries the
        // takeover of epoch 2; `fenced-2.jsonl` does not exist.
        std::fs::write(dir_path.join("fenced-1.jsonl"), b"{\"a\":1}\n").unwrap();
        assert!(!session_path.exists());

        move_aside_log(&session_path, &dir_path, 2).unwrap();

        assert_eq!(std::fs::read(&session_path).unwrap(), b"{\"a\":1}\n");
    }

    #[test]
    fn move_aside_log_rejects_an_oversized_source_without_truncating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap();
        let dir_path = lease_dir(&session_path);
        let oversized = otto_core::session::MAX_SESSION_FILE_BYTES as u64 + 1;
        std::fs::File::create(&session_path)
            .unwrap()
            .set_len(oversized)
            .unwrap();

        let err = move_aside_log(&session_path, &dir_path, 1).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains(&oversized.to_string()), "{msg}");
        assert!(
            msg.contains(&otto_core::session::MAX_SESSION_FILE_BYTES.to_string()),
            "{msg}"
        );

        let fenced = dir_path.join("fenced-1.jsonl");
        assert_eq!(
            std::fs::metadata(&fenced).unwrap().len(),
            oversized,
            "the fenced copy is left intact, not truncated"
        );
    }

    #[test]
    fn acquire_with_waits_out_a_released_heartbeat_behind_a_newer_epoch_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

        let keeper1 = no_op_keeper();
        let (lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();
        lease1.release().unwrap();

        // A concurrent acquirer created epoch 2 but has not written its
        // heartbeat yet: the released epoch-1 heartbeat is no longer the
        // newest marker.
        let dir_path = lease_dir(&session_path);
        let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
        create_epoch_marker(&dir, 2).unwrap();

        let clock2 = FakeClock::new();
        let keeper2 = no_op_keeper();
        let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

        assert_eq!(lease2.epoch(), 3);
        assert!(
            matches!(acquired, Acquired::TakenOver(_)),
            "expected TakenOver, got {acquired:?}"
        );
        assert!(clock2.sleep_count() >= 7, "expected to wait out 7L/6");
    }

    #[test]
    fn acquire_with_takes_over_a_stopped_holder_through_the_public_entry_point() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("s.jsonl");
        create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

        let keeper1 = no_op_keeper();
        let (_lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();
        let original = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3, incomplete";
        std::fs::write(&session_path, original).unwrap();

        let clock2 = FakeClock::new();
        let keeper2 = no_op_keeper();
        let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

        assert_eq!(lease2.epoch(), 2);
        assert!(
            matches!(acquired, Acquired::TakenOver(_)),
            "expected TakenOver, got {acquired:?}"
        );

        let dir_path = lease_dir(&session_path);
        assert_eq!(
            std::fs::read(dir_path.join("fenced-1.jsonl")).unwrap(),
            original,
            "the fenced copy keeps the original, untruncated bytes"
        );
        assert_eq!(
            std::fs::read(&session_path).unwrap(),
            b"{\"a\":1}\n{\"a\":2}\n",
            "the new log holds only the complete lines"
        );
    }
}
