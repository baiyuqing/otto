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

use crate::session::fsops;

#[cfg(test)]
use crate::child_fence::Children;

mod acquire;
mod fence;
mod heartbeat;
mod keeper;

pub(crate) use acquire::*;
pub(crate) use fence::new_keeper;
use heartbeat::*;
pub(crate) use keeper::*;

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
        crate::random::random_hex::<8>()?
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

pub fn local_hostname() -> String {
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

#[cfg(test)]
thread_local! {
    static SKIP_ACQUIRE_WAIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: runs `f` with every [`Lease::acquire`] on this thread waiting
/// out the liveness window on a virtual clock, so taking over a holder that
/// never renews costs no real time instead of 7`L`/6 (7 s at `L` = 6). Only
/// for a holder simulated by hand that stays stopped: a running holder's
/// heartbeat changes in real time, which the virtual window cannot observe.
#[cfg(test)]
pub(crate) fn without_acquire_wait<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            SKIP_ACQUIRE_WAIT.with(|skip| skip.set(self.0));
        }
    }
    let _reset = Reset(SKIP_ACQUIRE_WAIT.with(|skip| skip.replace(true)));
    f()
}

/// A [`Clock`] whose sleeps advance it instantly; see
/// [`without_acquire_wait`].
#[cfg(test)]
#[derive(Default)]
struct VirtualClock(Mutex<Duration>);

#[cfg(test)]
impl Clock for VirtualClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }

    fn sleep(&self, duration: Duration) {
        *self.0.lock().unwrap() += duration;
    }
}

/// A held session lease: one epoch, on one host, for one process.
pub struct Lease {
    _keeper: Arc<Keeper>,
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

impl std::fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Lease")
            .field("epoch", &self.epoch)
            .field("host", &self.host)
            .field("pid", &self.pid)
            .field("duration_secs", &self.duration_secs)
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// [`Lease::acquire_with_keeper`] with a keeper from [`new_keeper`]. Runtime
    /// repositories reuse their composed keeper instead.
    pub fn acquire(path: &Path) -> Result<(Arc<Lease>, Acquired), AcquireError> {
        Self::acquire_with_keeper(path, &new_keeper())
    }

    /// Acquires the lease for `session_path`'s lease directory (see
    /// [`create_lease_dir`]), waiting out a running holder's liveness
    /// window if needed. Registers the lease with the process-wide
    /// [`Keeper`], which renews it every `L`/3 and fences this process if
    /// renewal falls behind or a successor epoch appears.
    pub(crate) fn acquire_with_keeper(
        session_path: &Path,
        keeper: &Arc<Keeper>,
    ) -> Result<(Arc<Lease>, Acquired), AcquireError> {
        #[cfg(test)]
        if SKIP_ACQUIRE_WAIT.with(std::cell::Cell::get) {
            return acquire_with(session_path, &VirtualClock::default(), keeper);
        }
        acquire_with(session_path, &RealClock, keeper)
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
        if self.released.load(Ordering::Acquire) {
            return Err("session lease released".to_string());
        }
        if self.fenced.load(Ordering::Acquire) {
            let reason = self.lost_reason.lock().unwrap().clone().unwrap_or_default();
            return Err(format!("session lease lost: {reason}"));
        }
        if let Some(reason) = self.should_fence() {
            return Err(format!("session lease lost: {reason}"));
        }
        Ok(())
    }

    /// Serializes destructive cleanup against heartbeat writes. Success stops
    /// renewal without recreating a heartbeat in the removed lease directory.
    pub(crate) fn deleting<T>(
        &self,
        action: impl FnOnce() -> Result<T, otto_core::session::PiError>,
    ) -> Result<T, otto_core::session::PiError> {
        let _guard = self.write_lock.lock().unwrap();
        self.check().map_err(otto_core::session::PiError::other)?;
        let result = action();
        if result.is_ok() {
            self.released.store(true, Ordering::Release);
        }
        result
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
        if self.released.load(Ordering::Acquire)
            || self.superseded.load(Ordering::Acquire)
            || self.fenced.load(Ordering::Acquire)
        {
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

impl crate::session::WriterOwnership for Lease {
    fn check(&self) -> Result<(), String> {
        Lease::check(self)
    }

    fn release(&self) -> Result<(), String> {
        Lease::release(self).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
impl Lease {
    pub(crate) fn for_test(dir_path: &Path) -> Arc<Lease> {
        Self::for_test_with_duration(dir_path, DEFAULT_LEASE_SECONDS)
    }

    fn for_test_with_duration(dir_path: &Path, duration_secs: u64) -> Arc<Lease> {
        let dir = fsops::open_dir_no_follow(dir_path).expect("open test lease dir");
        let lease = Arc::new(Lease {
            _keeper: Keeper::new_for_test(
                Box::new(|| {}),
                Arc::new(Children::new()),
                Duration::from_millis(50),
            ),
            epoch: 1,
            host: "test-host".into(),
            pid: std::process::id(),
            duration_secs,
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

#[cfg(test)]
mod tests;
