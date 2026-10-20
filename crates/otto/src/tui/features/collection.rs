use chrono::Utc;
use otto_core::agent::context_report::{ContextReport, SectionKind};
use otto_core::model::Message;

use super::{Document, Presenter, Transition};
use crate::subagent::format::{first_runes, one_line};
use crate::subagent::record::{ListQuery, TaskRow};
use crate::tui::backend::{Reply, Request};
use crate::tui::interaction::{Action, Activation, Row, View};
use crate::tui::layout::{footer_workspace, format_token_count};

pub(crate) struct Context {
    report: Option<ContextReport>,
    expanded: Option<usize>,
}

impl Context {
    pub(crate) fn new() -> Self {
        Self {
            report: None,
            expanded: None,
        }
    }
}

impl Presenter for Context {
    fn key(&self) -> String {
        "context".into()
    }

    fn view(&self) -> View {
        let Some(report) = &self.report else {
            return View {
                collection: true,
                title: "Context".into(),
                description: "Loading context…".into(),
                loading: true,
                ..View::default()
            };
        };
        let mut title = if report.estimate_complete {
            format!(
                "Context  {} · ~{}",
                report.model,
                format_token_count(report.estimated_total)
            )
        } else {
            format!("Context  {} · estimate unknown", report.model)
        };
        if report.context_window > 0 {
            title.push_str(&format!(" / {}", format_token_count(report.context_window)));
        }
        if report.estimate_complete {
            title.push_str(" tokens (estimate)");
        }
        if let Some(tokens) = report.reported_input_tokens {
            title.push_str(&format!(" · last reported {}", format_token_count(tokens)));
        }
        if report.compaction_threshold > 0 {
            title.push_str(&format!(
                " · compacts at {}",
                format_token_count(report.compaction_threshold)
            ));
        }
        let total: i64 = report.sections.iter().map(|s| s.tokens.max(0)).sum();
        let mut rows = Vec::new();
        for (si, section) in report.sections.iter().enumerate() {
            let share_percent = if total > 0 {
                (section.tokens.max(0) as usize * 100 / total as usize).min(100)
            } else {
                0
            };
            rows.push(Row {
                id: serde_json::to_string(&("section", si, None::<usize>)).unwrap(),
                cells: vec![
                    section_name(section.kind, section.items.len()),
                    format_token_count(section.tokens),
                    format!("{share_percent}%"),
                ],
                detail: String::new(),
            });
            if self.expanded == Some(si) {
                for (ii, item) in section.items.iter().enumerate() {
                    rows.push(Row {
                        id: serde_json::to_string(&("item", si, Some(ii))).unwrap(),
                        cells: vec![item.label.clone(), format_token_count(item.tokens)],
                        detail: String::new(),
                    });
                }
            }
        }
        View {
            collection: true,
            title,
            rows,
            ..View::default()
        }
    }

    fn activate(&mut self, activation: Activation) -> Transition {
        let Activation::Open(id) = activation else {
            return Transition::Stay;
        };
        let Some(report) = &self.report else {
            return Transition::Stay;
        };
        let Ok(key) = serde_json::from_str::<(String, usize, Option<usize>)>(&id) else {
            return Transition::Stay;
        };
        if key.0 == "section" {
            self.expanded = (self.expanded != Some(key.1)).then_some(key.1);
            return Transition::Stay;
        }
        let Some(item) = key
            .2
            .and_then(|ii| report.sections.get(key.1)?.items.get(ii))
        else {
            return Transition::Stay;
        };
        Transition::Push(Box::new(Document {
            key: format!("context:{}", id),
            title: format!(
                "{} · ~{} tokens",
                item.label,
                format_token_count(item.tokens)
            ),
            text: item.text.clone(),
        }))
    }

    fn receive(&mut self, reply: Reply) -> Transition {
        if let Reply::Context(report) = reply {
            self.report = Some(report);
        }
        Transition::Stay
    }

    fn refresh(&self) -> Option<Request> {
        self.report.is_none().then_some(Request::Context)
    }
}

fn section_name(kind: SectionKind, count: usize) -> String {
    match kind {
        SectionKind::SystemPrompt => "System prompt".into(),
        SectionKind::Tools => format!("Tools (built-in, {count})"),
        SectionKind::McpTools => format!("Tools (MCP, {count})"),
        SectionKind::CompactionSummary => "Compaction summary".into(),
        SectionKind::Memory => "Memory (last turn)".into(),
        SectionKind::Messages => format!("Messages ({count})"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    All,
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
    Interrupted,
}
impl Status {
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
    fn query(self) -> Option<String> {
        (self != Self::All).then(|| self.label().to_string())
    }
}

pub(crate) struct Agents {
    workspace: String,
    all_workspaces: bool,
    status: Status,
    rows: Vec<TaskRow>,
    loading: bool,
    error: Option<String>,
}
impl Agents {
    pub(crate) fn new(workspace: String) -> Self {
        Self {
            workspace,
            all_workspaces: false,
            status: Status::All,
            rows: Vec::new(),
            loading: true,
            error: None,
        }
    }
    fn query(&self) -> Request {
        Request::Agents(ListQuery {
            status: self.status.query(),
            workspace: (!self.all_workspaces).then(|| self.workspace.clone()),
            limit: None,
            before: None,
        })
    }
}

impl Presenter for Agents {
    fn key(&self) -> String {
        "agents".into()
    }
    fn view(&self) -> View {
        let scope = if self.all_workspaces {
            "all workspaces"
        } else {
            "this workspace"
        };
        let mut title = format!("Agents  status:{} · {scope}", self.status.label());
        if let Some(error) = &self.error {
            title.push_str(&format!(" · {error}"));
        } else if self.rows.is_empty() && !self.loading {
            title.push_str(" · no recorded tasks");
        }
        View {
            collection: true,
            title,
            columns: [
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
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            loading: self.loading,
            rows: self
                .rows
                .iter()
                .map(|row| Row {
                    id: serde_json::to_string(&(&row.parent_session, &row.task_id)).unwrap(),
                    cells: columns(row).into(),
                    detail: format!(
                        "Task: {}\nStatus: {}\nWorkspace: {}\nParent: {}\nCreated: {}\nStarted: \
                        {}\nFinished: {}\nSteps: {} · Calls: {} · Tokens: {}\n\n{}",
                        row.task_id,
                        row.status,
                        row.workspace,
                        row.parent_session,
                        row.created_at,
                        row.started_at,
                        row.finished_at,
                        row.steps,
                        row.tool_calls,
                        format_token_count(row.input_tokens + row.output_tokens),
                        row.description
                    ),
                })
                .collect(),
            actions: vec![
                Action {
                    id: "status".into(),
                    label: format!("Status: {}", self.status.label()),
                    shortcut: Some('s'),
                    enabled: true,
                    ..Action::default()
                },
                Action {
                    id: "workspace".into(),
                    label: format!("Workspace: {scope}"),
                    shortcut: Some('w'),
                    enabled: true,
                    ..Action::default()
                },
            ],
            ..View::default()
        }
    }
    fn activate(&mut self, activation: Activation) -> Transition {
        match activation {
            Activation::Action { id, .. } if id == "status" => {
                self.status = self.status.next();
                self.loading = true;
                Transition::Request(self.query())
            }
            Activation::Action { id, .. } if id == "workspace" => {
                self.all_workspaces = !self.all_workspaces;
                self.loading = true;
                Transition::Request(self.query())
            }
            Activation::Open(id) => {
                if let Ok((parent, task)) = serde_json::from_str::<(String, String)>(&id)
                    && let Some(row) = self
                        .rows
                        .iter()
                        .find(|r| r.parent_session == parent && r.task_id == task)
                {
                    return Transition::Push(Box::new(AgentDetail::new(
                        row.clone(),
                        Vec::new(),
                        true,
                    )));
                }
                Transition::Stay
            }
            _ => Transition::Stay,
        }
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        if let Reply::Agents(result) = reply {
            self.rows = result.tasks;
            self.error = None;
            self.loading = false;
        }
        Transition::Stay
    }
    fn periodic(&self) -> bool {
        true
    }
    fn refresh(&self) -> Option<Request> {
        Some(self.query())
    }
}

fn columns(row: &TaskRow) -> [String; 10] {
    let agent = if row.agent.is_empty() {
        "default"
    } else {
        &row.agent
    };
    let desc = if row.description.is_empty() {
        first_runes(&one_line(&row.prompt), 60)
    } else {
        row.description.clone()
    };
    let duration = if row.started_at.is_empty() {
        "-".to_string()
    } else {
        let start = chrono::DateTime::parse_from_rfc3339(&row.started_at)
            .ok()
            .map(|time| time.timestamp());
        let end = if row.finished_at.is_empty() {
            Some(Utc::now().timestamp())
        } else {
            chrono::DateTime::parse_from_rfc3339(&row.finished_at)
                .ok()
                .map(|time| time.timestamp())
                .or(Some(Utc::now().timestamp()))
        };
        match start.zip(end) {
            Some((start, end)) => {
                let secs = (end - start).max(0);
                if secs < 60 {
                    format!("{secs}s")
                } else {
                    format!("{}m{}s", secs / 60, secs % 60)
                }
            }
            None => "-".into(),
        }
    };
    [
        row.status.clone(),
        agent.into(),
        desc,
        footer_workspace(&row.workspace),
        first_runes(&row.parent_session, 8),
        first_runes(&row.created_at, 16),
        duration,
        row.steps.to_string(),
        row.tool_calls.to_string(),
        format_token_count(row.input_tokens + row.output_tokens),
    ]
}

fn agent_document(row: TaskRow, history: Vec<Message>, missing: bool) -> Document {
    let mut text = format!(
        "Status: {}\nAgent: {}\nWorkspace: {}\nModel: {}\nCreated: {}\nStarted: {}\nFinished: {}\nSteps: {} · Calls: {} · Tokens: {}\n\nDescription\n{}\n\nPrompt\n{}\n\nResult\n{}\n\nError\n{}",
        row.status,
        if row.agent.is_empty() {
            "default"
        } else {
            &row.agent
        },
        row.workspace,
        row.model,
        row.created_at,
        row.started_at,
        row.finished_at,
        row.steps,
        row.tool_calls,
        format_token_count(row.input_tokens + row.output_tokens),
        row.description,
        row.prompt,
        row.result,
        row.error
    );
    text.push_str("\n\nTranscript\n");
    if missing {
        text.push_str("Transcript unavailable\n");
    } else if history.is_empty() {
        text.push_str("No transcript entries\n");
    } else {
        for entry in crate::tui::entries::entries_from_history(&history).0 {
            let label = match entry.kind {
                Some(super::super::entries::EntryKind::User) => "User",
                Some(super::super::entries::EntryKind::Assistant) => "Assistant",
                Some(super::super::entries::EntryKind::Reasoning) => "Reasoning",
                Some(super::super::entries::EntryKind::Tool) => "Tool",
                Some(super::super::entries::EntryKind::Compaction) => "Context checkpoint",
                Some(super::super::entries::EntryKind::Error) => "Error",
                _ => "Notice",
            };
            text.push_str(&format!("\n{label}\n{}", entry.raw));
            if !entry.tool_name.is_empty() {
                text.push_str(&format!(
                    "\nTool: {}\nArguments: {}\nOutput: {}",
                    entry.tool_name,
                    otto_core::wire::transcript::format_args(
                        otto_core::wire::events::tool_args_raw(&entry.tool_args).as_deref()
                    ),
                    entry.tool_output
                ));
            }
            text.push('\n');
        }
    }
    Document {
        key: format!("agent:{}:{}", row.parent_session, row.task_id),
        title: format!("Agent {} · {}", row.task_id, row.status),
        text,
    }
}

struct AgentDetail {
    loaded: bool,
    parent: String,
    id: String,
    document: Document,
}
impl AgentDetail {
    fn new(row: TaskRow, history: Vec<Message>, missing: bool) -> Self {
        Self {
            loaded: false,
            parent: row.parent_session.clone(),
            id: row.task_id.clone(),
            document: agent_document(row, history, missing),
        }
    }
}
impl Presenter for AgentDetail {
    fn key(&self) -> String {
        self.document.key.clone()
    }
    fn view(&self) -> View {
        let mut view = self.document.view();
        view.loading = !self.loaded;
        view
    }
    fn activate(&mut self, _: Activation) -> Transition {
        Transition::Stay
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        if let Reply::AgentDetail {
            row,
            history,
            missing,
        } = reply
        {
            self.document = agent_document(row, history, missing);
            self.loaded = true;
        }
        Transition::Stay
    }
    fn refresh(&self) -> Option<Request> {
        Some(Request::AgentDetail {
            parent: self.parent.clone(),
            id: self.id.clone(),
        })
    }
    fn periodic(&self) -> bool {
        true
    }
}
