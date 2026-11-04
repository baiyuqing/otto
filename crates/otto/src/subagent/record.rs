//! Domain contracts for persisted sub-agent task records.

#![allow(unsafe_code)]

use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::tasks::Task;

pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;

/// A storage failure carries no database text, paths, or row values across the
/// adapter boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("tasks store unavailable")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// The part of a row that is fixed for the life of one task registry: the
/// parent session it belongs to and the process that owns it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskContext {
    /// The parent session id, or `memory:<pid>:<process start RFC 3339>` for
    /// an in-memory parent.
    pub parent_session: String,
    pub parent_session_path: String,
    pub workspace: String,
    pub pid: i64,
    pub process_started_at: String,
}

/// Records one task's row after every lifecycle change. Implementations must
/// not fail their caller; [`Store`] disables itself on a write error instead.
pub trait Recorder: Send + Sync {
    fn upsert(&self, context: &TaskContext, task: &Task);
}

/// Reads persisted task snapshots for the task API and server views.
pub trait TaskReader: Send + Sync {
    fn list(&self, query: &ListQuery) -> Result<ListResult>;
    /// Observer metadata, bounded before decoding large payloads where supported.
    /// Compatibility backends use `list`; callers still enforce their final cap.
    fn list_observer(&self, query: &ListQuery) -> Result<ListResult> {
        self.list(query)
    }
    fn get(&self, parent_session: &str, task_id: &str) -> Result<Option<TaskRow>>;
}

/// The process-wide task repository used for both registry writes and API reads.
pub trait TaskRepository: Recorder + TaskReader {}

/// One row as read back, in the shape the server API serialises. `status` is
/// the displayed status (it can be `interrupted`); `cancelable` is not here
/// because only the server, which knows which sessions it owns, can compute
/// it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskRow {
    pub parent_session: String,
    pub task_id: String,
    pub workspace: String,
    pub parent_session_path: String,
    pub name: String,
    pub agent: String,
    pub description: String,
    pub model: String,
    pub model_source: super::tasks::ModelSource,
    pub context: String,
    pub prompt: String,
    pub status: String,
    pub created_at: String,
    pub started_at: String,
    pub finished_at: String,
    pub steps: i64,
    pub tool_calls: i64,
    pub last_tool: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub result: String,
    pub error: String,
    pub session_path: String,
}

/// A `GET /v1/tasks` query. `status` is one of the stored statuses or
/// `interrupted`; an unrecognised value matches no rows.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Restrict rows to one parent before ordering and limiting.
    pub parent_session: Option<String>,
    pub status: Option<String>,
    pub workspace: Option<String>,
    /// Defaults to 100, clamped to 500, when `None`.
    pub limit: Option<u32>,
    /// An exclusive `created_at` cursor.
    pub before: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ListResult {
    pub tasks: Vec<TaskRow>,
    /// Empty when there are no older rows.
    pub next_before: String,
}

/// Whether a queued/running row no longer has a verifiably live owner.
pub(crate) fn interrupted(pid: i64, process_started_at: &str) -> bool {
    let Ok(pid) = u32::try_from(pid) else {
        return true;
    };
    let Ok(recorded) = DateTime::parse_from_rfc3339(process_started_at) else {
        return true;
    };
    match os_process_start_time(pid) {
        Some(actual) => actual != recorded.with_timezone(&Utc),
        None => true,
    }
}

/// Approval recovery requires evidence of a dead/replaced owner, not a guess
/// from unavailable process telemetry.
#[allow(unsafe_code)]
pub(crate) fn process_matches(pid: i64, started: &str) -> Option<bool> {
    let pid_u32 = u32::try_from(pid).ok()?;
    let recorded = DateTime::parse_from_rfc3339(started).ok()?;
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
fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
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

#[cfg(target_os = "linux")]
fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
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
fn os_process_start_time(_pid: u32) -> Option<DateTime<Utc>> {
    None
}

pub use crate::storage::backends::sqlite::tasks::Store;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Connection;
    use crate::subagent::tasks::TaskStatus;

    fn context() -> TaskContext {
        TaskContext {
            parent_session: "s1".into(),
            parent_session_path: "/home/me/.otto/sessions/s1.jsonl".into(),
            workspace: "/work".into(),
            pid: 4_294_967_294, // a pid that cannot exist
            process_started_at: "2026-09-25T10:00:00Z".into(),
        }
    }

    fn task() -> Task {
        Task {
            id: "t1".into(),
            name: "reviewer".into(),
            agent: "code-reviewer".into(),
            description: "review the diff".into(),
            prompt: "review it".into(),
            context: "fresh".into(),
            model: "gpt-5.1".into(),
            status: TaskStatus::Running,
            created_at: DateTime::parse_from_rfc3339("2026-09-25T10:00:00Z")
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            started_at: DateTime::parse_from_rfc3339("2026-09-25T10:00:01Z")
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            steps: 3,
            tool_calls: 5,
            last_tool: "read".into(),
            session_path: "/home/me/.otto/sessions/s1/t1-child.jsonl".into(),
            ..Task::default()
        }
    }

    #[test]
    fn observer_parent_filter_is_applied_before_limit_for_final_and_dynamic_rows() {
        let store = Store::open_in_memory().expect("store");
        for status in [TaskStatus::Succeeded, TaskStatus::Running] {
            let mut selected = task();
            selected.status = status;
            store.upsert(&context(), &selected);
            let mut unrelated = context();
            unrelated.parent_session = "other".into();
            for index in 0..40 {
                let mut newer = selected.clone();
                newer.id = format!("t{index}");
                newer.created_at = selected
                    .created_at
                    .map(|time| time + chrono::Duration::seconds(index + 1));
                store.upsert(&unrelated, &newer);
            }
            let result = store
                .list(&ListQuery {
                    parent_session: Some("s1".into()),
                    limit: Some(1),
                    ..Default::default()
                })
                .expect("scoped list");
            assert_eq!(result.tasks.len(), 1);
            assert_eq!(result.tasks[0].parent_session, "s1");
            assert_eq!(result.tasks[0].task_id, "t1");
            assert!(result.next_before.is_empty());
        }
    }

    #[test]
    fn upsert_and_list_round_trip_a_task_row() {
        let store = Store::open_in_memory().expect("store");
        store.upsert(&context(), &task());

        let result = store.list(&ListQuery::default()).expect("list");
        assert_eq!(result.next_before, "");
        assert_eq!(result.tasks.len(), 1);
        let row = &result.tasks[0];
        assert_eq!(row.parent_session, "s1");
        assert_eq!(row.task_id, "t1");
        assert_eq!(row.workspace, "/work");
        assert_eq!(row.parent_session_path, "/home/me/.otto/sessions/s1.jsonl");
        assert_eq!(row.name, "reviewer");
        assert_eq!(row.agent, "code-reviewer");
        assert_eq!(row.description, "review the diff");
        assert_eq!(row.model, "gpt-5.1");
        assert_eq!(row.context, "fresh");
        assert_eq!(row.prompt, "review it");
        // A dead pid: displayed as interrupted even though stored as running.
        assert_eq!(row.status, "interrupted");
        assert_eq!(row.created_at, "2026-09-25T10:00:00.000000000Z");
        assert_eq!(row.finished_at, "");
        assert_eq!(row.steps, 3);
        assert_eq!(row.tool_calls, 5);
        assert_eq!(row.last_tool, "read");
        assert_eq!(
            row.session_path,
            "/home/me/.otto/sessions/s1/t1-child.jsonl"
        );

        let fetched = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(fetched, *row);
        assert_eq!(store.get("s1", "missing").expect("get"), None);
    }

    #[test]
    fn a_later_upsert_replaces_the_same_row() {
        let store = Store::open_in_memory().expect("store");
        store.upsert(&context(), &task());
        let finished = Task {
            status: TaskStatus::Succeeded,
            result: "done".into(),
            ..task()
        };
        store.upsert(&context(), &finished);

        let result = store.list(&ListQuery::default()).expect("list");
        assert_eq!(
            result.tasks.len(),
            1,
            "one row per (parent_session, task_id)"
        );
        assert_eq!(result.tasks[0].status, "succeeded");
        assert_eq!(result.tasks[0].result, "done");
    }

    #[test]
    fn prompt_result_and_error_are_truncated_at_64kib_on_a_char_boundary() {
        // 'é' is 2 bytes in UTF-8; MAX_TEXT_BYTES is even, so a run of them
        // lands a boundary exactly on the limit and truncation must not
        // split the last character.
        let long = "é".repeat(MAX_TEXT_BYTES / 2 + 10);
        let store = Store::open_in_memory().expect("store");
        let over_limit = Task {
            prompt: long.clone(),
            result: long.clone(),
            error: long.clone(),
            ..task()
        };
        store.upsert(&context(), &over_limit);

        let row = store.get("s1", "t1").expect("get").expect("row exists");
        for field in [&row.prompt, &row.result, &row.error] {
            assert!(field.len() <= MAX_TEXT_BYTES);
            assert!(long.starts_with(field.as_str()));
            assert!(field.is_char_boundary(field.len()));
        }
    }

    #[test]
    fn a_write_error_disables_the_recorder_after_one_failure() {
        let store = Store::open_in_memory().expect("store");
        store.break_schema_for_test();

        assert!(!store.is_disabled());
        store.upsert(&context(), &task());
        assert!(store.is_disabled(), "a write error must disable the store");
        // A second call after disabling must not panic or try the broken
        // connection again.
        store.upsert(&context(), &task());
        assert!(store.is_disabled());
    }

    #[test]
    fn an_unknown_user_version_is_not_opened() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("tasks.db");
        {
            let connection = Connection::open(&path).expect("create");
            connection
                .pragma_update("user_version", 999)
                .expect("set version");
        }
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn a_running_row_with_a_dead_pid_reads_as_interrupted() {
        let store = Store::open_in_memory().expect("store");
        store.upsert(&context(), &task());
        let row = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(row.status, "interrupted");
    }

    #[test]
    fn a_running_row_with_a_reused_pid_reads_as_interrupted() {
        let store = Store::open_in_memory().expect("store");
        let (pid, _) = current_process();
        let reused = TaskContext {
            pid,                                               // this test process, definitely alive
            process_started_at: "1999-01-01T00:00:00Z".into(), // definitely wrong
            ..context()
        };
        store.upsert(&reused, &task());
        let row = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(row.status, "interrupted");
    }

    #[test]
    fn a_finished_row_never_shows_interrupted() {
        let store = Store::open_in_memory().expect("store");
        let finished = Task {
            status: TaskStatus::Succeeded,
            ..task()
        };
        store.upsert(&context(), &finished); // context()'s pid cannot exist
        let row = store.get("s1", "t1").expect("get").expect("row exists");
        assert_eq!(row.status, "succeeded");
    }

    #[test]
    fn interrupted_filter_merges_final_and_owner_lost_rows_across_pages() {
        let store = Store::open_in_memory().expect("store");
        for (id, created, status) in [
            ("t1", "2026-09-25T10:00:00Z", TaskStatus::Interrupted),
            ("t2", "2026-09-25T10:01:00Z", TaskStatus::Running),
            ("t3", "2026-09-25T10:02:00Z", TaskStatus::Interrupted),
        ] {
            store.upsert(
                &context(),
                &Task {
                    id: id.into(),
                    status,
                    created_at: DateTime::parse_from_rfc3339(created)
                        .ok()
                        .map(|v| v.with_timezone(&Utc)),
                    ..Task::default()
                },
            );
        }
        let first = store
            .list(&ListQuery {
                status: Some("interrupted".into()),
                limit: Some(2),
                ..ListQuery::default()
            })
            .expect("first");
        assert_eq!(
            first
                .tasks
                .iter()
                .map(|v| v.task_id.as_str())
                .collect::<Vec<_>>(),
            ["t3", "t2"]
        );
        let second = store
            .list(&ListQuery {
                status: Some("interrupted".into()),
                before: Some(first.next_before),
                limit: Some(2),
                ..ListQuery::default()
            })
            .expect("second");
        assert_eq!(
            second
                .tasks
                .iter()
                .map(|v| v.task_id.as_str())
                .collect::<Vec<_>>(),
            ["t1"]
        );
        assert!(
            first
                .tasks
                .iter()
                .chain(&second.tasks)
                .all(|v| v.status == "interrupted")
        );
    }

    #[test]
    fn list_filters_by_status_workspace_and_paginates_with_before() {
        let store = Store::open_in_memory().expect("store");
        let (pid, started_at) = current_process();
        for (id, created_at, status, workspace) in [
            ("t1", "2026-09-25T10:00:00Z", TaskStatus::Succeeded, "/work"),
            ("t2", "2026-09-25T10:01:00Z", TaskStatus::Running, "/work"),
            ("t3", "2026-09-25T10:02:00Z", TaskStatus::Failed, "/other"),
            ("t4", "2026-09-25T10:03:00Z", TaskStatus::Succeeded, "/work"),
        ] {
            store.upsert(
                &TaskContext {
                    pid,
                    process_started_at: started_at.clone(),
                    workspace: workspace.into(),
                    ..context()
                },
                &Task {
                    id: id.into(),
                    status,
                    created_at: DateTime::parse_from_rfc3339(created_at)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc)),
                    ..Task::default()
                },
            );
        }

        let workspace_only = store
            .list(&ListQuery {
                workspace: Some("/work".into()),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(workspace_only.tasks.len(), 3);

        let running_only = store
            .list(&ListQuery {
                status: Some("running".into()),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(running_only.tasks.len(), 1);
        assert_eq!(running_only.tasks[0].task_id, "t2");

        let page = store
            .list(&ListQuery {
                limit: Some(2),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(page.tasks.len(), 2);
        assert_eq!(page.tasks[0].task_id, "t4");
        assert_eq!(page.tasks[1].task_id, "t3");
        assert_eq!(page.next_before, "2026-09-25T10:02:00.000000000Z");

        let next_page = store
            .list(&ListQuery {
                limit: Some(2),
                before: Some(page.next_before.clone()),
                ..ListQuery::default()
            })
            .expect("list");
        assert_eq!(next_page.tasks.len(), 2);
        assert_eq!(next_page.tasks[0].task_id, "t2");
        assert_eq!(next_page.tasks[1].task_id, "t1");
        assert_eq!(next_page.next_before, "");
    }
}
