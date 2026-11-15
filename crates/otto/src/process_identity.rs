//! Process identity: the pid with its start time, and whether a recorded
//! identity still names a live process. Used for ownership claims whose owner
//! may have exited and for task records of an interrupted process.

#![allow(unsafe_code)]

use chrono::{DateTime, SecondsFormat, Utc};

/// Approval recovery requires evidence of a dead/replaced owner, not a guess
/// from unavailable process telemetry.
#[allow(unsafe_code)]
pub(crate) fn process_matches(pid: i64, started: &str) -> Option<bool> {
    let pid_u32 = u32::try_from(pid).ok()?;
    let recorded = DateTime::parse_from_rfc3339(started).ok()?;
    // A zombie has exited: it runs no code and holds no file open. It keeps
    // its pid until its parent waits for it, so `kill(pid, 0)` below would
    // still succeed for it.
    if os_process_is_zombie(pid_u32) {
        return Some(false);
    }
    if let Some(actual) = os_process_start_time(pid_u32) {
        return Some(actual == recorded.with_timezone(&Utc));
    }
    // SAFETY: signal 0 only tests process existence; it sends no signal.
    let status = unsafe { libc::kill(i32::try_from(pid).ok()?, 0) };
    if status < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Some(false)
    } else {
        None
    }
}

/// The current process's pid and start time, for [`TaskContext::pid`] and
/// [`TaskContext::process_started_at`].
pub fn current_process() -> (i64, String) {
    let pid = std::process::id();
    // ponytail: when the platform cannot report a start time (see
    // `os_process_start_time`), fall back to now so the row is still
    // written; every later liveness check for this pid will also fail to
    // confirm a match and read the row as `interrupted`, which is the honest
    // answer on a platform this build cannot introspect.
    let started = os_process_start_time(pid).unwrap_or_else(Utc::now);
    (
        i64::from(pid),
        started.to_rfc3339_opts(SecondsFormat::Nanos, true),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    DateTime::from_timestamp(
        info.pbi_start_tvsec as i64,
        (info.pbi_start_tvusec as u32).saturating_mul(1000),
    )
}

/// Whether `pid` names a process that has exited and has not been waited for.
///
/// For a zombie, `proc_pidinfo(PROC_PIDTBSDINFO)` returns 0 with `ESRCH`
/// while `kill(pid, 0)` returns 0 (measured on macOS 27: `sysctl
/// kern.proc.pid` reports `p_stat == SZOMB` for the same pid). A pid that
/// names no process fails both calls with `ESRCH`; a running process
/// answers `proc_pidinfo`.
#[cfg(target_os = "macos")]
fn os_process_is_zombie(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable `proc_bsdinfo` of `size` bytes.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if written == size || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        return false;
    }
    // SAFETY: signal 0 only tests process existence; it sends no signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Whether `pid` names a process that has exited and has not been waited for:
/// state `Z` (zombie) or `X` (dead) in `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn os_process_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.split_whitespace().next());
    matches!(state, Some("Z" | "X"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_process_is_zombie(_pid: u32) -> bool {
    false
}

#[cfg(target_os = "linux")]
pub(crate) fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    // /proc/{pid}/stat's comm field (2nd, parenthesised) may itself contain
    // spaces or parentheses, so the real fields start after the last ") ".
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    // state=0 ppid=1 pgrp=2 session=3 tty_nr=4 tpgid=5 flags=6 minflt=7
    // cminflt=8 majflt=9 cmajflt=10 utime=11 stime=12 cutime=13 cstime=14
    // priority=15 nice=16 num_threads=17 itrealvalue=18 starttime=19
    let starttime_ticks: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    let clock_ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if clock_ticks_per_second <= 0 {
        return None;
    }
    let boot = linux_boot_time()?;
    let offset_millis =
        (starttime_ticks as f64 / clock_ticks_per_second as f64 * 1000.0).round() as i64;
    boot.checked_add_signed(chrono::Duration::milliseconds(offset_millis))
}

#[cfg(target_os = "linux")]
fn linux_boot_time() -> Option<DateTime<Utc>> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let btime = stat.lines().find_map(|line| line.strip_prefix("btime "))?;
    DateTime::from_timestamp(btime.trim().parse().ok()?, 0)
}

/// otto only ships a confined driver for macOS and runs unsandboxed on
/// Linux (see `AGENTS.md`); on any other platform a `queued`/`running` row
/// simply cannot be confirmed alive and always reads as `interrupted`.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn os_process_start_time(_pid: u32) -> Option<DateTime<Utc>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exited_child_that_was_not_waited_for_does_not_match() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = i64::from(child.id());
        // `true` exits at once; it stays a zombie until `wait` below.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !os_process_is_zombie(child.id()) {
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let started = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        assert_eq!(process_matches(pid, &started), Some(false));
        child.wait().expect("wait");
    }

    #[test]
    fn the_current_process_matches_and_is_not_a_zombie() {
        let (pid, started) = current_process();
        assert!(!os_process_is_zombie(std::process::id()));
        assert_eq!(process_matches(pid, &started), Some(true));
    }
}
