//! Session failover: an epoch-based file lease that lets Otto continue a
//! session on another host, and the mechanics used to enforce it.
//!
//! `lease` owns the lease protocol: acquire, hold, check, release, built on
//! `crate::session::fsops`'s `openat` primitives. `children` tracks process
//! ids this process started, so a fenced process can kill them before it
//! stops. This module also defines
//! [`CommitGuard`], a `tool::registry::CallGuard` that keeps a
//! lease-managed session's on-disk state ahead of a tool result reaching
//! the session log: it refuses a call when the lease has been lost, and
//! syncs the workspace to disk once a call finishes.
//!
//! Ownership: `CommitGuard` holds a [`LeaseSource`] closure rather than a
//! lease directly, and re-reads the current lease at call time, since the
//! lease backing a session can be acquired, released, or lost
//! independently of the guard's own lifetime. Concurrency: `before`/`after`
//! run on the calling task and do not block beyond the syscalls they
//! invoke. Errors: both hooks return `Err(String)` with operator-facing
//! text, which `crate::tool::registry::Registry::execute` folds into the
//! tool result.

pub mod children;
pub mod lease;
pub mod recovery;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tool::registry::CallGuard;

/// Produces the lease currently backing a session, or `None` when the
/// session is not lease-managed. Called once per tool call by
/// [`CommitGuard`].
pub type LeaseSource = Arc<dyn Fn() -> Option<Arc<lease::Lease>> + Send + Sync>;

/// Linux: `syncfs(2)` on the file system holding `path`. Other platforms:
/// returns `Ok(())` without syncing; the commit rule applies on Linux only.
#[cfg(target_os = "linux")]
pub fn syncfs(path: &Path) -> std::io::Result<()> {
    #![allow(unsafe_code)]
    use std::os::fd::AsRawFd;
    let dir = std::fs::File::open(path)?;
    // SAFETY: dir.as_raw_fd() is open for the duration of the call.
    let result = unsafe { libc::syncfs(dir.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn syncfs(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

type SyncFn = fn(&Path) -> std::io::Result<()>;

/// A [`CallGuard`] enforcing the commit rule for a lease-managed session:
/// `before` fails once the lease has been lost, so a tool never runs
/// against a session another host may already have taken over; `after`
/// syncs the workspace to disk once the tool has finished, so a
/// successfully returned result's effects are durable before it is
/// appended to the session log.
pub struct CommitGuard {
    lease: LeaseSource,
    workspace: PathBuf,
    sync: SyncFn,
}

impl CommitGuard {
    /// `lease` is queried at every call; `workspace` is the directory
    /// synced after a tool call finishes.
    pub fn new(lease: LeaseSource, workspace: PathBuf) -> Self {
        CommitGuard::with_sync(lease, workspace, syncfs)
    }

    fn with_sync(lease: LeaseSource, workspace: PathBuf, sync: SyncFn) -> Self {
        CommitGuard {
            lease,
            workspace,
            sync,
        }
    }
}

impl CallGuard for CommitGuard {
    fn before(&self) -> Result<(), String> {
        match (self.lease)() {
            Some(lease) => lease.check(),
            None => Ok(()),
        }
    }

    fn after(&self) -> Result<(), String> {
        if (self.lease)().is_none() {
            return Ok(());
        }
        (self.sync)(&self.workspace).map_err(|error| {
            format!(
                "tool call finished but syncing the workspace to disk failed, so its effects may not be durable: {error}"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    use otto_core::model::{EffectCertainty, OperationDisposition, OperationId, ToolDefinition};
    use otto_core::tool::{ToolCall, ToolExecutor, ToolResult};
    use serde_json::value::RawValue;
    use tokio_util::sync::CancellationToken;

    use crate::tool::registry::Registry;
    use crate::tool::{Tool, definition, text_result};

    struct FakeTool;

    #[async_trait::async_trait]
    impl Tool for FakeTool {
        fn definition(&self) -> ToolDefinition {
            definition("write", "", serde_json::json!({"type": "object"}))
        }

        async fn execute(&self, _arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
            text_result("wrote")
        }
    }

    fn no_lease() -> LeaseSource {
        Arc::new(|| None)
    }

    fn noop_sync(_p: &Path) -> std::io::Result<()> {
        Ok(())
    }

    #[test]
    fn before_and_after_are_no_ops_without_a_lease() {
        let guard = CommitGuard::with_sync(no_lease(), PathBuf::from("/tmp"), noop_sync);

        assert!(guard.before().is_ok());
        assert!(guard.after().is_ok());
    }

    #[test]
    fn before_reports_a_lost_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = lease::Lease::for_test(tmp.path());
        lease.mark_lost_for_test("test fence");
        let source: LeaseSource = Arc::new(move || Some(Arc::clone(&lease)));
        let guard = CommitGuard::new(source, PathBuf::from("/tmp"));

        let err = guard.before().unwrap_err();
        assert!(err.starts_with("session lease lost"), "got: {err}");
    }

    #[test]
    fn after_syncs_the_workspace_when_a_lease_backs_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = lease::Lease::for_test(tmp.path());
        let source: LeaseSource = Arc::new(move || Some(Arc::clone(&lease)));

        static SYNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn fake_sync(_p: &Path) -> std::io::Result<()> {
            SYNCED.store(true, Ordering::Relaxed);
            Ok(())
        }
        let guard = CommitGuard::with_sync(source, PathBuf::from("/workspace"), fake_sync);

        assert!(guard.after().is_ok());
        assert!(SYNCED.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn sync_failure_settles_the_registry_call_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = lease::Lease::for_test(tmp.path());
        let source: LeaseSource = Arc::new(move || Some(Arc::clone(&lease)));
        fn failing_sync(_p: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
        let registry = Registry::new(vec![Box::new(FakeTool)])
            .unwrap()
            .with_guard(Arc::new(CommitGuard::with_sync(
                source,
                PathBuf::from("/workspace"),
                failing_sync,
            )));
        let operation_id: OperationId = serde_json::from_str(r#""op_commit_guard_test""#).unwrap();
        let arguments = RawValue::from_string("{}".to_owned()).unwrap();

        let execution = registry
            .execute(
                ToolCall {
                    operation_id: &operation_id,
                    name: "write",
                    arguments: &arguments,
                    attempt: 1,
                },
                &CancellationToken::new(),
            )
            .await;

        assert!(execution.result.is_error);
        assert_eq!(execution.outcome.disposition, OperationDisposition::Error);
        assert_eq!(execution.outcome.effect_certainty, EffectCertainty::Unknown);
    }

    #[test]
    fn after_wraps_a_sync_failure_with_operator_facing_text() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = lease::Lease::for_test(tmp.path());
        let source: LeaseSource = Arc::new(move || Some(Arc::clone(&lease)));
        fn failing_sync(_p: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
        let guard = CommitGuard::with_sync(source, PathBuf::from("/workspace"), failing_sync);

        let err = guard.after().unwrap_err();
        assert!(
            err.contains("syncing the workspace to disk failed"),
            "got: {err}"
        );
        assert!(err.contains("disk full"), "got: {err}");
    }
}
