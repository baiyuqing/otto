//! The `/agents` overlay: every recorded sub-agent task in `tasks.db`,
//! across every session and process (see
//! `docs/specs/2026-09-25-agents-view.md`, "TUI"). Unlike
//! [`super::context_view::ContextView`], which is built once from a
//! snapshot, this overlay re-queries [`Controller::builder`]'s
//! [`crate::subagent::record::Store`] on every filter change and on the
//! caller's periodic tick, since the underlying rows change while the
//! overlay is open.

use chrono::{DateTime, Utc};
use crossterm::event::KeyCode;

use super::entries::{self, Entry};
use super::layout::{footer_workspace, format_token_count};
use crate::app::Controller;
use crate::subagent::format::{first_runes, one_line};
use crate::subagent::record::{ListQuery, TaskRow};

/// The column headers, in the order [`columns`] returns them.
pub(crate) const COLUMN_HEADERS: [&str; 10] = [
    "Status",
    "Agent",
    "Description",
    "Workspace",
    "Parent",
    "Created",
    "Duration",
    "Steps",
    "Calls",
    "Tokens",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusFilter {
    All,
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
    Interrupted,
}

impl StatusFilter {
    /// `s` cycles through these seven values in this order (spec order).
    fn next(self) -> Self {
        match self {
            Self::All => Self::Queued,
            Self::Queued => Self::Running,
            Self::Running => Self::Succeeded,
            Self::Succeeded => Self::Failed,
            Self::Failed => Self::Canceled,
            Self::Canceled => Self::Interrupted,
            Self::Interrupted => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }

    /// The `ListQuery::status` value this filter maps to; `None` for `All`.
    fn query_value(self) -> Option<&'static str> {
        (self != Self::All).then(|| self.label())
    }
}

/// The selected task's detail pane: its own row plus its child transcript.
#[derive(Debug, Clone)]
pub(crate) struct Detail {
    pub row: TaskRow,
    pub entries: Vec<Entry>,
    pub transcript_missing: bool,
    pub scroll: u16,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentsView {
    pub rows: Vec<TaskRow>,
    pub status: StatusFilter,
    pub this_workspace_only: bool,
    workspace: String,
    pub selected: usize,
    pub detail: Option<Detail>,
    pub error: Option<String>,
}

impl AgentsView {
    /// Opens the overlay, scoped to `controller`'s workspace by default, and
    /// runs the first query.
    pub fn open(controller: &Controller) -> Self {
        let mut view = Self {
            rows: Vec::new(),
            status: StatusFilter::All,
            this_workspace_only: true,
            workspace: controller.workspace().to_string(),
            selected: 0,
            detail: None,
            error: None,
        };
        view.refresh(controller);
        view
    }

    fn query(&self) -> ListQuery {
        ListQuery {
            status: self.status.query_value().map(str::to_string),
            workspace: self.this_workspace_only.then(|| self.workspace.clone()),
            limit: None,
            before: None,
        }
    }

    /// Re-lists rows for the current filters, keeping the same row selected
    /// (by parent session and task id) when it is still in the result.
    fn refresh(&mut self, controller: &Controller) {
        let selected_key = self.rows.get(self.selected).map(row_key);
        match controller.builder().tasks_list(&self.query()) {
            Ok(result) => {
                self.rows = result.tasks;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        self.selected = selected_key
            .and_then(|key| self.rows.iter().position(|row| row_key(row) == key))
            .unwrap_or(0);
    }

    /// The periodic refresh: re-lists while the list is showing, or
    /// re-fetches the open task while the detail pane is showing.
    pub fn tick(&mut self, controller: &Controller) {
        match self.detail.as_ref().map(|detail| row_key(&detail.row)) {
            Some((parent_session, task_id)) => {
                self.refresh_detail(controller, &parent_session, &task_id);
            }
            None => self.refresh(controller),
        }
    }

    fn refresh_detail(&mut self, controller: &Controller, parent_session: &str, task_id: &str) {
        if let Ok(Some(row)) = controller.builder().tasks_get(parent_session, task_id) {
            let scroll = self.detail.as_ref().map_or(0, |detail| detail.scroll);
            self.detail = Some(load_detail(row, scroll));
        }
    }

    fn open_detail(&mut self, controller: &Controller) {
        let Some(selected) = self.rows.get(self.selected).cloned() else {
            return;
        };
        let row = match controller
            .builder()
            .tasks_get(&selected.parent_session, &selected.task_id)
        {
            Ok(Some(fresh)) => fresh,
            _ => selected,
        };
        self.detail = Some(load_detail(row, 0));
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.rows.len() as isize;
        if len > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        }
    }

    /// Applies one key. Returns `false` when Esc closes the overlay
    /// (from the list; Esc from the detail pane goes back to the list).
    pub fn handle_key(&mut self, code: KeyCode, controller: &Controller) -> bool {
        if let Some(detail) = &mut self.detail {
            match code {
                KeyCode::Esc => self.detail = None,
                KeyCode::Up => detail.scroll = detail.scroll.saturating_sub(1),
                KeyCode::Down => detail.scroll = detail.scroll.saturating_add(1),
                KeyCode::PageUp => detail.scroll = detail.scroll.saturating_sub(10),
                KeyCode::PageDown => detail.scroll = detail.scroll.saturating_add(10),
                _ => {}
            }
            return true;
        }
        match code {
            KeyCode::Esc => return false,
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Char('s') => {
                self.status = self.status.next();
                self.refresh(controller);
            }
            KeyCode::Char('w') => {
                self.this_workspace_only = !self.this_workspace_only;
                self.refresh(controller);
            }
            KeyCode::Enter => self.open_detail(controller),
            _ => {}
        }
        true
    }

    /// The overlay title: the active filters, and an empty-result or error
    /// note.
    pub fn header(&self) -> String {
        let scope = if self.this_workspace_only {
            "this workspace"
        } else {
            "all workspaces"
        };
        let mut header = format!("Agents  status:{} · {scope}", self.status.label());
        if let Some(error) = &self.error {
            header.push_str(&format!(" · {error}"));
        } else if self.rows.is_empty() {
            header.push_str(" · no recorded tasks");
        }
        header
    }
}

fn row_key(row: &TaskRow) -> (String, String) {
    (row.parent_session.clone(), row.task_id.clone())
}

fn load_detail(row: TaskRow, scroll: u16) -> Detail {
    let (entries, transcript_missing) = if row.session_path.is_empty() {
        (Vec::new(), true)
    } else {
        match crate::session::Store::read_transcript(&row.session_path) {
            Ok(history) => (entries::entries_from_history(&history).0, false),
            Err(_) => (Vec::new(), true),
        }
    };
    Detail {
        row,
        entries,
        transcript_missing,
        scroll,
    }
}

fn short_session(id: &str) -> String {
    if id.is_empty() {
        String::new()
    } else {
        format!("#{}", first_runes(id, 8))
    }
}

fn duration_label(row: &TaskRow, now: DateTime<Utc>) -> String {
    if row.started_at.is_empty() {
        return "-".to_string();
    }
    let Ok(start) = DateTime::parse_from_rfc3339(&row.started_at) else {
        return "-".to_string();
    };
    let start = start.with_timezone(&Utc);
    let end = if row.finished_at.is_empty() {
        now
    } else {
        DateTime::parse_from_rfc3339(&row.finished_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(now)
    };
    let seconds = (end - start).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

/// One row's rendered columns, in [`COLUMN_HEADERS`] order: status, agent
/// (or the literal `default`), description (falling back to the prompt's
/// first line when unset), workspace basename, a short parent-session id,
/// created time, duration, steps, tool calls, and tokens (input + output).
pub(crate) fn columns(row: &TaskRow, now: DateTime<Utc>) -> [String; 10] {
    let agent = if row.agent.is_empty() {
        "default".to_string()
    } else {
        row.agent.clone()
    };
    let description = if row.description.is_empty() {
        first_runes(&one_line(&row.prompt), 60)
    } else {
        row.description.clone()
    };
    [
        row.status.clone(),
        agent,
        description,
        footer_workspace(&row.workspace),
        short_session(&row.parent_session),
        first_runes(&row.created_at, 16),
        duration_label(row, now),
        row.steps.to_string(),
        row.tool_calls.to_string(),
        format_token_count(row.input_tokens + row.output_tokens),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use chrono::TimeZone;

    use super::*;
    use crate::cli::testutil;
    use crate::subagent::record::{self, TaskContext};
    use crate::subagent::tasks::{Task, TaskStatus};

    #[test]
    fn status_filter_cycles_through_all_seven_values_in_spec_order() {
        let mut status = StatusFilter::All;
        let mut labels = vec![status.label()];
        for _ in 0..6 {
            status = status.next();
            labels.push(status.label());
        }
        assert_eq!(
            labels,
            [
                "all",
                "queued",
                "running",
                "succeeded",
                "failed",
                "canceled",
                "interrupted"
            ]
        );
        assert_eq!(status.next(), StatusFilter::All, "the cycle wraps");
    }

    #[test]
    fn columns_fall_back_to_default_agent_and_a_prompt_derived_description() {
        let row = TaskRow {
            agent: String::new(),
            description: String::new(),
            prompt: "review the diff for correctness\nsecond line".into(),
            workspace: "/Users/me/src/app".into(),
            parent_session: "01J234567890".into(),
            steps: 3,
            tool_calls: 5,
            input_tokens: 12_000,
            output_tokens: 800,
            status: "running".into(),
            created_at: "2026-09-25T10:00:00Z".into(),
            ..TaskRow::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 9, 25, 10, 0, 30).unwrap();
        let columns = columns(&row, now);
        assert_eq!(columns[0], "running");
        assert_eq!(columns[1], "default");
        assert_eq!(columns[2], "review the diff for correctness second line");
        assert_eq!(columns[3], "app");
        assert_eq!(columns[4], "#01J23456");
        assert_eq!(columns[6], "-", "no started_at yet");
        assert_eq!(columns[7], "3");
        assert_eq!(columns[8], "5");
    }

    #[test]
    fn duration_formats_seconds_then_minutes_and_seconds() {
        let mut row = TaskRow {
            started_at: "2026-09-25T10:00:00Z".into(),
            finished_at: "2026-09-25T10:00:45Z".into(),
            ..TaskRow::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 9, 25, 11, 0, 0).unwrap();
        assert_eq!(duration_label(&row, now), "45s");

        row.finished_at = "2026-09-25T10:02:05Z".into();
        assert_eq!(duration_label(&row, now), "2m5s");

        row.finished_at.clear();
        let running_now = Utc.with_ymd_and_hms(2026, 9, 25, 10, 1, 0).unwrap();
        assert_eq!(duration_label(&row, running_now), "1m0s");
    }

    /// A controller whose builder carries a task recorder, mirroring
    /// `cli::repl_commands::tests::controller_with_task_recorder`.
    async fn controller_with_task_recorder(
        workspace: &Path,
        sessions: &Path,
        store: Arc<record::Store>,
    ) -> Controller {
        let mut builder = testutil::builder(workspace, sessions);
        builder.shared_mut().task_recorder = Some(store);
        let runtime = testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        Controller::new(builder, true, session, runner, info)
    }

    fn seed(
        store: &record::Store,
        workspace: &str,
        parent_session: &str,
        task_id: &str,
        status: TaskStatus,
    ) {
        let context = TaskContext {
            parent_session: parent_session.into(),
            parent_session_path: format!("/sessions/{parent_session}.jsonl"),
            workspace: workspace.into(),
            pid: 4_294_967_294, // a pid that cannot exist: never "running" by liveness
            process_started_at: "2026-09-25T10:00:00Z".into(),
        };
        record::Recorder::upsert(
            store,
            &context,
            &Task {
                id: task_id.into(),
                description: format!("task {task_id}"),
                status,
                created_at: Some(Utc::now()),
                ..Task::default()
            },
        );
    }

    #[tokio::test]
    async fn open_lists_rows_for_this_workspace_only_by_default() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = Arc::new(record::Store::open_in_memory().expect("store"));
        let workspace_path = workspace.path().to_str().expect("utf8 path").to_string();
        seed(&store, &workspace_path, "s1", "t1", TaskStatus::Succeeded);
        seed(&store, "/elsewhere", "s2", "t2", TaskStatus::Succeeded);
        let controller =
            controller_with_task_recorder(workspace.path(), sessions.path(), Arc::clone(&store))
                .await;

        let view = AgentsView::open(&controller);
        assert_eq!(view.rows.len(), 1, "only the current workspace's row");
        assert_eq!(view.rows[0].task_id, "t1");
    }

    #[tokio::test]
    async fn w_toggles_between_this_workspace_and_all_workspaces() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = Arc::new(record::Store::open_in_memory().expect("store"));
        let workspace_path = workspace.path().to_str().expect("utf8 path").to_string();
        seed(&store, &workspace_path, "s1", "t1", TaskStatus::Succeeded);
        seed(&store, "/elsewhere", "s2", "t2", TaskStatus::Succeeded);
        let controller =
            controller_with_task_recorder(workspace.path(), sessions.path(), Arc::clone(&store))
                .await;

        let mut view = AgentsView::open(&controller);
        assert!(view.handle_key(KeyCode::Char('w'), &controller));
        assert_eq!(view.rows.len(), 2, "both workspaces now listed");
        assert!(view.handle_key(KeyCode::Char('w'), &controller));
        assert_eq!(view.rows.len(), 1, "back to this workspace only");
    }

    #[tokio::test]
    async fn s_cycles_the_status_filter_and_requeries() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = Arc::new(record::Store::open_in_memory().expect("store"));
        let workspace_path = workspace.path().to_str().expect("utf8 path").to_string();
        seed(&store, &workspace_path, "s1", "t1", TaskStatus::Succeeded);
        seed(&store, &workspace_path, "s1", "t2", TaskStatus::Failed);
        let controller =
            controller_with_task_recorder(workspace.path(), sessions.path(), Arc::clone(&store))
                .await;

        let mut view = AgentsView::open(&controller);
        assert_eq!(view.rows.len(), 2);
        assert!(view.handle_key(KeyCode::Char('s'), &controller));
        assert_eq!(view.status, StatusFilter::Queued);
        assert!(view.rows.is_empty(), "no queued rows");

        for _ in 0..3 {
            view.handle_key(KeyCode::Char('s'), &controller);
        }
        assert_eq!(view.status, StatusFilter::Failed);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].task_id, "t2");
    }

    #[tokio::test]
    async fn enter_opens_the_detail_pane_and_esc_goes_back_then_closes() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = Arc::new(record::Store::open_in_memory().expect("store"));
        let workspace_path = workspace.path().to_str().expect("utf8 path").to_string();
        seed(&store, &workspace_path, "s1", "t1", TaskStatus::Succeeded);
        let controller =
            controller_with_task_recorder(workspace.path(), sessions.path(), Arc::clone(&store))
                .await;

        let mut view = AgentsView::open(&controller);
        assert!(view.handle_key(KeyCode::Enter, &controller));
        let detail = view.detail.as_ref().expect("detail pane open");
        assert_eq!(detail.row.task_id, "t1");
        assert!(detail.transcript_missing, "no session_path was recorded");

        assert!(
            view.handle_key(KeyCode::Esc, &controller),
            "back to the list"
        );
        assert!(view.detail.is_none());
        assert!(
            !view.handle_key(KeyCode::Esc, &controller),
            "Esc on the list closes the overlay"
        );
    }

    #[tokio::test]
    async fn up_and_down_move_selection_and_wrap() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = Arc::new(record::Store::open_in_memory().expect("store"));
        let workspace_path = workspace.path().to_str().expect("utf8 path").to_string();
        seed(&store, &workspace_path, "s1", "t1", TaskStatus::Succeeded);
        seed(&store, &workspace_path, "s1", "t2", TaskStatus::Succeeded);
        let controller =
            controller_with_task_recorder(workspace.path(), sessions.path(), Arc::clone(&store))
                .await;

        let mut view = AgentsView::open(&controller);
        assert_eq!(view.selected, 0);
        view.handle_key(KeyCode::Up, &controller);
        assert_eq!(view.selected, 1, "up from the first row wraps to the last");
        view.handle_key(KeyCode::Down, &controller);
        assert_eq!(view.selected, 0);
    }

    #[tokio::test]
    async fn no_recorder_reports_no_recorded_tasks_in_the_header() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;

        let view = AgentsView::open(&controller);
        assert!(view.rows.is_empty());
        assert!(
            view.header().contains("no recorded tasks"),
            "{}",
            view.header()
        );
    }
}
