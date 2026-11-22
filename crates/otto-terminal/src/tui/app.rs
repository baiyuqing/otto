//! TUI state and composition. Terminal input is translated to semantic intents
//! by `input`; this module owns the feature stack and transcript.
use super::commands::Completion as Suggestion;
use super::{
    attach::Remote,
    backend::{self, Completion, Driver, Reply, Request, Ticket},
    entries::{self, Entry, EntryKind},
    features::{self, Presenter, Run, Transition},
    interaction::{self, Activation, View},
    selection::Selection,
    widgets::Geometry,
};
use crossterm::event::{KeyCode, KeyEvent};
use interaction::composer::{Composer, Edit};
use otto::{
    app::{Controller, Info},
    subagent::tasks::Task,
};
use otto_core::{
    model::{Message, Usage},
    tool::ToolResult,
    wire::{
        events::{APPROVAL_DECIDED, APPROVAL_REQUESTED, USER_MESSAGE, WireEvent},
        transcript,
    },
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    future,
    ops::{Deref, DerefMut},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

mod completion;
mod events;
mod keys;
mod lifecycle;
mod observer;
mod requests;
mod submission;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatus {
    pub phase: String,
    pub turn_elapsed: Duration,
}
const CTRL_C_ARM_WINDOW: Duration = Duration::from_secs(1);
const CTRL_C_EXIT_STATUS: &str = "press Ctrl+C again to exit";

#[derive(Clone, Copy)]
pub(crate) enum Backend<'a> {
    Local(&'a Controller),
    Attach(&'a Remote),
}
impl Backend<'_> {
    pub(crate) fn info(&self) -> Info {
        match self {
            Self::Local(c) => c.info(),
            Self::Attach(r) => r.info(),
        }
    }
    fn history(&self) -> Vec<Message> {
        match self {
            Self::Local(c) => c.history(),
            Self::Attach(r) => r.history(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Exit,
    Prompt(String),
    LocalPrompt {
        text: String,
        input_id: String,
        delivery: otto_core::model::InputDelivery,
    },
    Compact(String),
    Login(String),
    McpLogin(String),
    CancelTurn,
}

pub(crate) struct App {
    pub entries: Vec<Entry>,
    pub usage: Usage,
    /// Whether every accumulated usage report included a cache breakdown.
    pub cache_breakdown_complete: Option<bool>,
    pub info: Info,
    pub composer: Composer,
    pub queued_input: Option<String>,
    pub(super) queued_input_id: Option<String>,
    pub(super) input_statuses: std::collections::HashMap<String, String>,
    pub(super) next_input_delivery: Option<otto_core::model::InputDelivery>,
    /// The latest successful model or reasoning selection, kept as one
    /// single-line item per banner row instead of appended to the transcript.
    pub composer_notices: Vec<String>,
    /// The newest guidance the active run has queued, painted only from a
    /// back-end read: the TUI never guesses what the run accepted.
    pub queued_steer: Option<otto_core::agent::steering::Message>,
    /// Every queued guidance item, in admission order; transient display only.
    pub pending_steers: Vec<otto_core::agent::steering::Message>,
    steering_generation: Option<u64>,
    /// IDs already represented by a delivery notification in this lifecycle.
    delivered_steers: HashSet<u64>,
    pub new_messages: bool,
    /// Guidance Ctrl+U asked the runtime to withdraw. Its text returns to an
    /// empty composer only after the runtime reports it as withdrawn.
    recalling_steer: Option<otto_core::agent::steering::Message>,
    awaiting_user_echo: Option<String>,
    optimistic_user_echo: Option<String>,
    pub scroll: Option<u16>,
    pub max_scroll: std::cell::Cell<u16>,
    /// Rendered transcript rows kept between frames; synced by the renderer.
    pub transcript_cache: std::cell::RefCell<super::transcript::render_cache::RenderCache>,
    pub selection: Option<Selection>,
    pub show_details: bool,
    pub tasks: Vec<Task>,
    pub observer: super::observer::Observer,
    observer_refreshed: Option<Instant>,
    pub(crate) attached: bool,
    pub driver: Option<Driver>,
    pub geometry: RefCell<Geometry>,
    pub surfaces: interaction::Host<Box<dyn Presenter>>,
    pub connected: bool,
    catalog: features::Catalog,
    generations: HashMap<u64, u64>,
    pending: HashSet<(u64, u64)>,
    writes: HashSet<(u64, u64)>,
    session_changes: HashSet<(u64, u64)>,
    /// Request tickets whose successful receipts belong above the composer.
    composer_feedback: HashSet<(u64, u64)>,
    requests: Vec<(Ticket, Request)>,
    actions: Vec<Action>,
    busy_since: Option<Instant>,
    phase: (String, Instant),
    retry_event: Option<WireEvent>,
    pub status: Option<String>,
    ctrl_c_armed_at: Option<Instant>,
    skill_names: Vec<String>,
    hidden_approvals: HashSet<String>,
    /// The last approval snapshot, kept for a surface that defers approvals.
    approval_snapshot: Vec<otto_core::approval::Request>,
    /// Approvals that arrived as events while a surface deferred them.
    deferred_approvals: Vec<features::Approval>,
    pub outgoing: Vec<Action>,
    pub(super) program_status: super::program_status::Status,
}
impl Deref for App {
    type Target = Composer;
    fn deref(&self) -> &Composer {
        &self.composer
    }
}
impl DerefMut for App {
    fn deref_mut(&mut self) -> &mut Composer {
        &mut self.composer
    }
}

fn skill_state_completions(name: &str, prefix: &str) -> Vec<Suggestion> {
    ["enabled", "disabled"]
        .into_iter()
        .filter(|state| state.starts_with(prefix))
        .map(|state| Suggestion {
            replacement: format!("/skill set {name} {state}"),
            description: "set skill state".to_string(),
        })
        .collect()
}

impl App {
    pub fn new(backend: &Backend<'_>) -> Self {
        Self::with_catalog(backend, features::Catalog::default())
    }
    pub(crate) fn with_catalog(backend: &Backend<'_>, catalog: features::Catalog) -> Self {
        let (mut entries, usage) = entries::entries_from_history(&backend.history());
        entries::project_local_commands(&mut entries, &backend.info().local_commands);
        let composer = Composer::new(prompt_history(&entries));
        let cache_breakdown_complete =
            (usage.input_tokens != 0 || usage.output_tokens != 0).then_some(false);
        let mut app = Self {
            entries,
            usage,
            cache_breakdown_complete,
            info: backend.info(),
            composer,
            queued_input: None,
            next_input_delivery: None,
            queued_input_id: None,
            input_statuses: Default::default(),
            composer_notices: Vec::new(),
            queued_steer: None,
            pending_steers: Vec::new(),
            steering_generation: None,
            delivered_steers: HashSet::new(),
            new_messages: false,
            recalling_steer: None,
            awaiting_user_echo: None,
            optimistic_user_echo: None,
            scroll: None,
            max_scroll: Default::default(),
            transcript_cache: Default::default(),
            selection: None,
            show_details: false,
            tasks: Vec::new(),
            observer: Default::default(),
            observer_refreshed: None,
            attached: matches!(backend, Backend::Attach(_)),
            driver: None,
            geometry: Default::default(),
            surfaces: Default::default(),
            connected: true,
            catalog,
            generations: HashMap::new(),
            pending: HashSet::new(),
            writes: HashSet::new(),
            session_changes: HashSet::new(),
            composer_feedback: HashSet::new(),
            requests: Vec::new(),
            actions: Vec::new(),
            busy_since: None,
            phase: ("".into(), Instant::now()),
            retry_event: None,
            status: None,
            ctrl_c_armed_at: None,
            skill_names: Vec::new(),
            hidden_approvals: HashSet::new(),
            approval_snapshot: Vec::new(),
            deferred_approvals: Vec::new(),
            outgoing: Vec::new(),
            program_status: super::program_status::Status::default(),
        };
        app.refresh_tasks(backend);
        app
    }
}

fn append_stream(entries: &mut Vec<Entry>, id: &str, kind: EntryKind, text: &str) {
    // Status notices may arrive between chunks of the same provider reply.
    // Only content rows or an explicitly closed stream delimit its output.
    if let Some(last) = entries
        .iter_mut()
        .rev()
        .find(|entry| entry.kind != Some(EntryKind::System))
        .filter(|e| e.kind == Some(kind) && e.id == id)
    {
        last.raw.push_str(text)
    } else {
        entries.push(Entry {
            id: id.into(),
            kind: Some(kind),
            raw: text.into(),
            ..Entry::default()
        })
    }
}
/// Close live rows without dropping their contents or informational notices.
fn close_streams(entries: &mut [Entry]) {
    for (index, entry) in entries.iter_mut().enumerate().rev() {
        if matches!(entry.id.as_str(), "streaming" | "streaming-reasoning") {
            entry.id = format!("completed-stream-{index}");
        } else if entry.kind != Some(EntryKind::System) {
            break;
        }
    }
}

/// The prompts a session already had, oldest first, for the composer's recall.
fn prompt_history(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.kind == Some(EntryKind::User))
        .map(|e| e.raw.clone())
        .collect()
}
struct OperationReceipt {
    message: String,
    info: Info,
    history: Option<Vec<Message>>,
    prompt: Option<String>,
    /// Text placed in the editor, not sent.
    draft: Option<String>,
}
fn operation_receipt(reply: &Reply) -> Option<OperationReceipt> {
    match reply {
        Reply::Operation {
            message,
            info,
            history,
            prompt,
        } => Some(OperationReceipt {
            message: message.clone(),
            info: info.clone(),
            history: history.clone(),
            prompt: prompt.clone(),
            draft: None,
        }),
        Reply::Forked {
            message,
            info,
            history,
            draft,
        } => Some(OperationReceipt {
            message: message.clone(),
            info: info.clone(),
            history: Some(history.clone()),
            prompt: None,
            draft: draft.clone(),
        }),
        _ => None,
    }
}
pub(crate) fn compaction_line(result: &otto_core::wire::events::WireCompaction) -> String {
    if result.noop {
        return "[context] no-op".into();
    }
    match (result.tokens_before, result.estimated_tokens_after) {
        (Some(before), Some(after)) => format!(
            "[context] compacted {} → {} tokens",
            super::layout::format_token_count(before),
            super::layout::format_token_count(after)
        ),
        (Some(before), None) => format!(
            "[context] compacted {} tokens",
            super::layout::format_token_count(before)
        ),
        (None, _) => "[context] compacted".into(),
    }
}

#[cfg(test)]
mod tests;
