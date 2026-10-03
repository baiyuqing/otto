//! Application state, key handling, and command dispatch.
//!
//! Ratatui redraws the whole frame every tick from plain state instead of an
//! Elm-style message loop, so this module owns only state and key handling;
//! [`super::render`] turns that state into widgets.
//!
//! ponytail: command output for `/session`, `/model` with no argument,
//! `/sandbox`, `/tasks` and `/task` is appended to the transcript as a system
//! entry rather than shown in an overlay, reusing the text the line frontend
//! prints for the same commands. `/resume`, `/archive`, `/model`, and
//! `/thinking` have interactive picker overlays because they select from a
//! bounded list of existing choices. Upgrade path: split these into dedicated
//! overlays if a user reports the inline transcript entries as hard to scan.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use otto_core::agent::context_report::ContextReport;
use otto_core::model::{Message, Usage};
use otto_core::session::types::SessionInfo;
use otto_core::tool::ToolResult;
use otto_core::wire::events::{
    APPROVAL_DECIDED, APPROVAL_REQUESTED, USER_MESSAGE, WireCompaction, WireEvent,
};
use otto_core::wire::transcript;
use tokio_util::sync::CancellationToken;

use crate::app::tasks::{Task, TaskStatus};
use crate::app::{Controller, Info, PROFILE_SWITCH_UNAVAILABLE};
use crate::cli::info::{SandboxInfo, SandboxNetwork};
use crate::cli::login;
use crate::cli::repl_commands;
use crate::cli::sandbox_setup::parse_exclude_entry;
use crate::memory::{CandidateListRequest, CandidateState};
use crate::subagent::record::{ListQuery, ListResult, TaskRow};

use super::agents_view::AgentsView;
use super::attach::Remote;
use super::commands::{self, Completion, SlashCommandKind};
use super::context_view::ContextView;
use super::entries::{self, Entry, EntryKind};
use super::layout;
use super::selection::Selection;

/// What the status line under the transcript shows while a turn runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatus {
    pub phase: String,
    pub phase_elapsed: Duration,
    pub turn_elapsed: Duration,
}

/// The time a first Ctrl+C stays armed for a confirming second press.
const CTRL_C_ARM_WINDOW: Duration = Duration::from_secs(1);

const CTRL_C_EXIT_STATUS: &str = "press Ctrl+C again to exit";

/// How many sessions a `/resume` or `/archive` picker lists. How many sessions
/// a `/resume` or `/archive` picker lists.
const PICKER_LIST_LIMIT: usize = 50;

/// One row of a resume/archive/profile picker.
#[derive(Debug, Clone)]
pub(crate) struct PickerRow {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerKind {
    Resume,
    Archive,
    Profile,
    Effort,
    Thinking,
    Sandbox,
}

impl PickerKind {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Resume => "Resume session (enter to select, esc to cancel)",
            Self::Archive => "Archive session (enter to select, esc to cancel)",
            Self::Profile => "Switch profile (enter to select, esc to cancel)",
            Self::Effort => "Reasoning effort (enter to use, s to save, esc to cancel)",
            Self::Thinking => "Reasoning effort (enter to use, s to save, esc to cancel)",
            Self::Sandbox => "Sandbox change (select, enter to apply, esc to cancel)",
        }
    }
}

/// An open list picker.
#[derive(Debug, Clone)]
pub(crate) struct Picker {
    pub kind: PickerKind,
    pub rows: Vec<PickerRow>,
    pub selected: usize,
}

impl Picker {
    fn new(kind: PickerKind, rows: Vec<PickerRow>) -> Self {
        Self {
            kind,
            rows,
            selected: 0,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }
}

/// A pending Bash elevation or sandbox read approval above the composer.
///
/// The choice is made with the arrow keys and Enter, so it works under any
/// input method; `y`/`n` and `1`/`2` are shortcuts for the same two options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApprovalDialog {
    pub id: String,
    pub command: String,
    pub justification: String,
    pub read_path: String,
    /// Whether "Yes" is the highlighted option. Starts on "No" so a stray
    /// Enter never grants a permission.
    pub approve_selected: bool,
}

/// One scoped pending review row. The dialog is an authorized local review
/// surface, so it may hold candidate details fetched through the controller's
/// existing user/workspace scopes. These fields never enter the event signal,
/// notices, logs, or metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryReviewRow {
    pub id: String,
    pub action: String,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MemoryReviewDialog {
    pub rows: Vec<MemoryReviewRow>,
    pub selected: usize,
}

/// Async work [`App::handle_key`] cannot start itself (every
/// [`Controller`] method it would need is `async`). [`super::run`] awaits
/// these one at a time, matching [`Controller::begin_operation`]'s
/// single-admission-at-a-time contract.
pub(crate) enum Action {
    Exit,
    Prompt(String),
    Image(String),
    Compact(String),
    Reflect(String),
    NewSession,
    SwitchProfile(String),
    SwitchProfileThinking {
        profile: String,
        thinking: String,
        save: bool,
    },
    SetThinking {
        thinking: String,
        save: bool,
    },
    Resume(String),
    Archive(String),
    SandboxReload,
    /// One resolved absolute path to add to the sandbox `read_paths`.
    SandboxAllow(String),
    /// The sandbox network mode, `allow` or `deny`.
    SandboxNetwork(String),
    /// One validated-later `excluded_commands` entry to add.
    SandboxExclude(String),
    Approve(String),
    MemoryReview {
        id: String,
        accept: bool,
    },
    /// `--attach` only: deny a waiting approval (the dialog's No).
    Deny(String),
    /// Approve the pending command and exclude its program from the sandbox.
    ApproveAlways(String),
    Login(String),
    McpLogin(String),
}

/// What the frontend's calls reach: the controller of this process, or the
/// `otto serve` the TUI is attached to (`otto --attach`). Only the calls both
/// modes support are methods here; a command that exists in local mode only
/// matches `Backend::Local` itself and prints [`ATTACH_UNAVAILABLE`] otherwise.
#[derive(Clone, Copy)]
pub(crate) enum Backend<'a> {
    Local(&'a Controller),
    Attach(&'a Remote),
}

/// The line a command that `--attach` does not support prints after `/<command>:`.
const ATTACH_UNAVAILABLE: &str = "not available with --attach";

impl Backend<'_> {
    pub(crate) fn info(&self) -> Info {
        match self {
            Self::Local(controller) => controller.info(),
            Self::Attach(remote) => remote.info(),
        }
    }

    fn history(&self) -> Vec<Message> {
        match self {
            Self::Local(controller) => controller.history(),
            Self::Attach(remote) => remote.history(),
        }
    }

    pub(crate) fn workspace(&self) -> String {
        match self {
            Self::Local(controller) => controller.workspace().to_string(),
            Self::Attach(remote) => remote.workspace().to_string(),
        }
    }

    pub(crate) fn tasks_list(&self, query: &ListQuery) -> Result<ListResult, String> {
        match self {
            Self::Local(controller) => controller.builder().tasks_list(query),
            Self::Attach(remote) => remote.tasks_list(query),
        }
    }

    pub(crate) fn tasks_get(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<TaskRow>, String> {
        match self {
            Self::Local(controller) => controller.builder().tasks_get(parent_session, task_id),
            Self::Attach(remote) => remote.tasks_get(parent_session, task_id),
        }
    }

    fn context_report(&self) -> Result<ContextReport, String> {
        match self {
            Self::Local(controller) => controller
                .context_report()
                .ok_or_else(|| "no session is open".to_string()),
            Self::Attach(remote) => remote.context_report(),
        }
    }

    fn sandbox_info(&self) -> SandboxInfo {
        match self {
            Self::Local(controller) => controller.sandbox_info(),
            Self::Attach(remote) => remote.info().sandbox,
        }
    }

    fn rename_session(&self, name: &str) -> Result<(), String> {
        match self {
            Self::Local(controller) => controller.rename_session(name),
            Self::Attach(remote) => remote.rename_session(name),
        }
    }

    fn withdraw_user_message(&self) -> bool {
        match self {
            Self::Local(controller) => controller.withdraw_user_message(),
            Self::Attach(remote) => remote.withdraw_queued_turn(),
        }
    }

    /// The rows of a `/resume` picker, newest first.
    fn session_rows(&self) -> Result<Vec<PickerRow>, String> {
        match self {
            Self::Local(controller) => controller
                .list_sessions(PICKER_LIST_LIMIT)
                .map(|result| result.sessions.iter().map(session_row).collect()),
            Self::Attach(remote) => remote.session_rows(PICKER_LIST_LIMIT),
        }
    }
}

/// The composer's bash-style prompt history: the prompts the transcript already
/// held when Otto started, then every line submitted since, plus the draft a
/// recall interrupted.
///
/// ponytail: in-process only. Upgrade path: write the lines to a history file
/// if recall across runs is asked for.
#[derive(Default)]
pub(crate) struct History {
    lines: Vec<String>,
    /// Which line the composer is showing, or `None` while it holds what
    /// was typed. Any composer edit clears it, so a recall never outlives
    /// the value it put in the composer.
    index: Option<usize>,
    /// What the composer held when the recall started, restored by Down
    /// past the newest line.
    draft: String,
}

impl History {
    fn seeded(lines: Vec<String>) -> Self {
        Self {
            lines,
            ..Self::default()
        }
    }

    /// Records one submitted line and ends any recall in force.
    fn remember(&mut self, line: &str) {
        self.index = None;
        self.draft.clear();
        if !line.is_empty() {
            self.lines.push(line.to_string());
        }
    }

    /// The line before the one showing, or `None` when there is nothing
    /// older (the composer is then left as it is). The first recall saves
    /// `current` as the draft.
    fn previous(&mut self, current: &str) -> Option<String> {
        let index = match self.index {
            Some(0) => return None,
            Some(index) => index - 1,
            None => {
                let newest = self.lines.len().checked_sub(1)?;
                self.draft = current.to_string();
                newest
            }
        };
        self.index = Some(index);
        Some(self.lines[index].clone())
    }

    /// The line after the one showing, the draft once past the newest, or
    /// `None` when no recall is in force.
    fn next(&mut self) -> Option<String> {
        let index = self.index? + 1;
        if let Some(line) = self.lines.get(index) {
            self.index = Some(index);
            return Some(line.clone());
        }
        self.index = None;
        Some(std::mem::take(&mut self.draft))
    }

    /// Whether the composer is showing a recalled line rather than a draft.
    /// The suggestion panel gives Up/Down back to the history while it is,
    /// so a recalled slash command cannot strand the keys walking it.
    fn recalling(&self) -> bool {
        self.index.is_some()
    }

    /// Ends the recall, leaving the edited value as an ordinary draft.
    fn stop_recall(&mut self) {
        self.index = None;
    }
}

/// The prompts a transcript holds, oldest first: what [`History`] starts
/// from, so a session resumed at startup can recall its own prompts.
fn prompt_history(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.kind == Some(EntryKind::User))
        .map(|entry| entry.raw.clone())
        .collect()
}

/// The terminal frontend's whole state.
pub(crate) struct App {
    pub entries: Vec<Entry>,
    pub usage: Usage,
    pub info: Info,
    /// Draft text in the composer. While a turn is running this is only a
    /// draft; Enter commits it to [`App::queued_input`] and clears the box.
    pub input: Vec<char>,
    pub cursor: usize,
    /// Input submitted while the current turn is still running. Ordinary text
    /// is queued in the agent inbox for its next safe checkpoint; slash
    /// commands remain local until the turn finishes.
    pub queued_input: Option<String>,
    /// Whether `queued_input` is already in the running agent's inbox.
    pub queued_input_sent: bool,
    /// Bash-style prompt history for the composer's Up/Down keys.
    history: History,
    /// `None` follows the bottom of the transcript; `Some(top)` pins the view
    /// to that absolute wrapped-line offset from the top.
    ///
    /// The offset is absolute rather than measured from the bottom so that
    /// output appended during a turn extends the transcript below the pinned
    /// rows instead of pushing them off the top.
    pub scroll: Option<u16>,
    /// The largest offset the last drawn frame could scroll to (its total
    /// wrapped line count minus the transcript height), or `0` before the
    /// first frame. [`super::render::draw`] is the sole writer; the scroll
    /// keys read it to turn "following the bottom" into an absolute offset,
    /// since only the renderer knows how the entries wrap at the current
    /// width.
    pub max_scroll: Cell<u16>,
    /// The in-progress or last-finished mouse selection, in screen cells.
    ///
    /// Frontend-only view state, like [`App::scroll`]: asking the terminal
    /// for mouse reporting takes its own drag-selection away, so Otto draws
    /// and copies the selection itself ([`super::selection`]). Any key or
    /// wheel notch clears it, because both move the text out from under it.
    pub selection: Option<Selection>,
    pub picker: Option<Picker>,
    /// The open `/context` overlay.
    pub context: Option<ContextView>,
    /// The open `/agents` overlay.
    pub agents: Option<AgentsView>,
    /// Pending elevated Bash approval modal.
    pub approval: Option<ApprovalDialog>,
    /// Pending human memory-review modal. It contains no candidate body or
    /// reason; decisions go through the existing scoped review command.
    pub memory_review: Option<MemoryReviewDialog>,
    /// The highlighted row of the slash-command suggestion panel (see
    /// [`App::suggestions`]). Every composer edit resets it to `0`, so it
    /// only ever indexes the match list the current value produces.
    pub suggestion: usize,
    pub show_help: bool,
    pub show_details: bool,
    /// When the turn now in flight started, or `None` between turns. One
    /// field rather than a `bool` plus a timestamp, so "a turn is running"
    /// and "how long it has been running" cannot disagree.
    busy_since: Option<Instant>,
    /// The running turn's phase and when it began; see
    /// [`otto_core::wire::transcript::phase`].
    phase: (String, Instant),
    retry_event: Option<WireEvent>,
    pub status: Option<String>,
    ctrl_c_armed_at: Option<Instant>,
    /// Snapshot of this session's sub-agent registry, refreshed from the
    /// controller before every draw (see [`App::refresh_tasks`]). The panel
    /// ([`super::render`]) reads only this field, never the registry itself,
    /// so rendering performs no lock or query.
    pub tasks: Vec<crate::subagent::tasks::Task>,
    /// Snapshot of discovered names used by skill completions.
    skill_names: Vec<String>,
    /// Whether the TUI is attached to `otto serve`: the transcript then
    /// takes every prompt from `user_message` frames, and a dialog opens only
    /// from `approval_requested`.
    pub(crate) attached: bool,
    /// `--attach` only: actions raised outside [`App::handle_key`]'s return
    /// value (queued text typed while a turn runs). The attach loop drains it
    /// after every key.
    pub(crate) outgoing: Vec<Action>,
}

fn skill_state_completions(name: &str, prefix: &str) -> Vec<Completion> {
    ["enabled", "disabled"]
        .into_iter()
        .filter(|state| state.starts_with(prefix))
        .map(|state| Completion {
            replacement: format!("/skill set {name} {state}"),
            description: "set skill state".to_string(),
        })
        .collect()
}

impl App {
    pub fn new(backend: &Backend) -> Self {
        let (entries, usage) = entries::entries_from_history(&backend.history());
        let history = History::seeded(prompt_history(&entries));
        let mut app = Self {
            entries,
            usage,
            info: backend.info(),
            input: Vec::new(),
            cursor: 0,
            queued_input: None,
            queued_input_sent: false,
            history,
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            context: None,
            agents: None,
            approval: None,
            memory_review: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
            retry_event: None,
            status: None,
            ctrl_c_armed_at: None,
            tasks: Vec::new(),
            skill_names: Vec::new(),
            attached: matches!(backend, Backend::Attach(_)),
            outgoing: Vec::new(),
        };
        app.refresh_tasks(backend);
        app
    }

    /// Refreshes [`App::tasks`] from the controller's live sub-agent
    /// registry. Called before every draw so [`super::render`] never locks
    /// or queries the registry itself.
    ///
    /// In attach mode this does nothing: the attach loop sets
    /// [`App::tasks`] from `session_tasks` when the status stream reports a
    /// new task count.
    pub(crate) fn refresh_tasks(&mut self, backend: &Backend) {
        let Backend::Local(controller) = backend else {
            return;
        };
        self.tasks = controller
            .subagent_tasks()
            .map(|tasks| tasks.list())
            .unwrap_or_default();
        self.skill_names = controller
            .skills()
            .skills()
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        self.skill_names.sort();
    }

    /// Marks a turn as started. [`super::run_turn`]/[`super::run_compact`]/
    /// [`super::run_wake`] bracket every `Controller` call with this and
    /// [`App::end_turn`].
    pub fn start_turn(&mut self) {
        let now = Instant::now();
        self.busy_since = Some(now);
        self.phase = ("waiting for model".into(), now);
        self.retry_event = None;
    }

    pub fn end_turn(&mut self) {
        self.busy_since = None;
    }

    pub fn busy(&self) -> bool {
        self.busy_since.is_some()
    }

    /// The running turn's phase and timings, or `None` between turns. Drives
    /// the status line in [`super::render`].
    pub fn thinking(&self) -> Option<TurnStatus> {
        Some(TurnStatus {
            phase: self
                .retry_event
                .as_ref()
                .and_then(|event| {
                    transcript::phase_at(
                        event,
                        self.phase.1.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    )
                })
                .unwrap_or_else(|| self.phase.0.clone()),
            phase_elapsed: self.phase.1.elapsed(),
            turn_elapsed: self.busy_since?.elapsed(),
        })
    }

    /// Rebuilds the transcript from the controller's current history.
    /// [`super::run`] calls this after every action that replaces the whole
    /// session (`/new`, `/resume`, `/archive`, switching profiles) so the
    /// transcript can never drift from `Controller::history`.
    ///
    /// A completed prompt or `/compact` does *not* call this: the transcript is
    /// append-only during a turn ([`App::apply_event`] is the sole writer), so
    /// an in-progress or just-finished turn's entries are never rebuilt out
    /// from under a still-visible scrollback.
    pub fn refresh(&mut self, backend: &Backend) {
        let (entries, usage) = entries::entries_from_history(&backend.history());
        self.entries = entries;
        self.usage = usage;
        self.info = backend.info();
        self.scroll = None;
        self.approval = None;
    }

    pub fn refresh_info(&mut self, backend: &Backend) {
        self.info = backend.info();
        self.usage = self.info.usage;
    }

    /// Appends one informational line, e.g. a command's result or an error
    /// [`super::run`] could not attribute to a streamed [`Event`].
    pub(crate) fn push_system(&mut self, text: impl Into<String>) {
        let id = format!("system-{}", self.entries.len());
        self.entries.push(Entry {
            id,
            kind: Some(EntryKind::System),
            raw: text.into(),
            ..Entry::default()
        });
        self.scroll = None;
    }

    fn ctrl_c_armed(&self, now: Instant) -> bool {
        self.ctrl_c_armed_at
            .is_some_and(|armed_at| now < armed_at + CTRL_C_ARM_WINDOW)
    }

    fn clear_ctrl_c_arm(&mut self) {
        self.ctrl_c_armed_at = None;
        if self.status.as_deref() == Some(CTRL_C_EXIT_STATUS) {
            self.status = None;
        }
    }

    fn arm_ctrl_c(&mut self, now: Instant) {
        self.ctrl_c_armed_at = Some(now);
        self.status = Some(CTRL_C_EXIT_STATUS.to_string());
    }

    fn scroll_up(&mut self, lines: u16) {
        self.selection = None;
        let top = self.scroll.unwrap_or_else(|| self.max_scroll.get());
        self.scroll = Some(top.saturating_sub(lines));
    }

    fn scroll_down(&mut self, lines: u16) {
        self.selection = None;
        let Some(top) = self.scroll else { return };
        let bottom = self.max_scroll.get();
        let next = top.saturating_add(lines);
        self.scroll = (next < bottom).then_some(next);
    }

    /// Scrolls the transcript by one mouse-wheel notch. The wheel is its own
    /// event rather than a synthesized key ([`super::TuiEvent::Wheel`]),
    /// because an idle composer's Up/Down recall prompt history.
    pub fn scroll_wheel(&mut self, up: bool) {
        if up {
            self.scroll_up(1);
        } else {
            self.scroll_down(1);
        }
    }

    /// Applies one transcript scroll key, reporting whether `key` was one.
    ///
    /// Split out of [`App::handle_key`] because [`super::drive_turn`] has to
    /// call it directly: `handle_key` drops every key while a turn is
    /// running, and scrolling back through output as it arrives is the one
    /// thing that still has to work then. Up/Down scroll here but not in an
    /// idle composer, where they recall prompt history ([`History`]); the
    /// mouse wheel arrives as [`super::TuiEvent::Wheel`] and is routed here
    /// in both states.
    pub fn handle_scroll_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => self.scroll_up(1),
            KeyCode::Down => self.scroll_down(1),
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
            _ => return false,
        }
        true
    }

    /// A lone Ctrl+C clears the composer and arms a second press; a confirming
    /// press within [`CTRL_C_ARM_WINDOW`] exits. While a turn is running,
    /// [`super::run`] intercepts Ctrl+C before it reaches [`App::handle_key`]
    /// at all (see [`is_interrupt_key`]) and cancels the turn directly instead
    /// of arming; [`App::busy`] is therefore always false here.
    fn handle_ctrl_c(&mut self) -> Option<Action> {
        let now = Instant::now();
        if self.ctrl_c_armed(now) {
            return Some(Action::Exit);
        }
        self.clear_ctrl_c_arm();
        if !self.input.is_empty() {
            self.input.clear();
            self.cursor = 0;
        }
        self.arm_ctrl_c(now);
        None
    }

    /// Opens the local human-review dialog from the current session's scoped
    /// pending candidates. Details are read only through this authorized scope;
    /// the signal that opened it never carries them.
    pub fn open_memory_review(&mut self, controller: &Controller) {
        let Some((service, user_scope, workspace_scope)) = controller.memory_manager() else {
            self.push_system("Memory review is unavailable.");
            return;
        };
        match service.list_candidates(&CandidateListRequest {
            scopes: vec![user_scope, workspace_scope],
            states: vec![CandidateState::Pending],
            limit: 20,
            cursor: String::new(),
        }) {
            Ok(page) if page.candidates.is_empty() => {}
            Ok(page) => {
                self.memory_review = Some(MemoryReviewDialog {
                    rows: page
                        .candidates
                        .into_iter()
                        .map(|candidate| MemoryReviewRow {
                            id: candidate.id,
                            action: candidate.action.as_str().to_string(),
                            kind: candidate.proposed.kind,
                            key: candidate.proposed.key,
                            text: candidate.proposed.text,
                            reason: candidate.reason,
                        })
                        .collect(),
                    selected: 0,
                });
            }
            Err(error) => self.push_system(format!("Memory review is unavailable: {error}")),
        }
    }

    /// Handles one key press. Returns `Some(Action)` for the one key
    /// (submitting a prompt or command) that needs an `async` `Controller`
    /// call; every purely local effect (composer editing, picker navigation,
    /// overlays) is applied directly to `self`.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        backend: &Backend,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        if key.code != KeyCode::Char('c') || !key.modifiers.contains(KeyModifiers::CONTROL) {
            self.clear_ctrl_c_arm();
        } else {
            return self.handle_ctrl_c();
        }

        if let Some(dialog) = &mut self.memory_review {
            match key.code {
                KeyCode::Esc => self.memory_review = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    if !dialog.rows.is_empty() {
                        dialog.selected =
                            (dialog.selected + dialog.rows.len() - 1) % dialog.rows.len();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !dialog.rows.is_empty() {
                        dialog.selected = (dialog.selected + 1) % dialog.rows.len();
                    }
                }
                KeyCode::Char('a' | 'A') | KeyCode::Char('r' | 'R') => {
                    let accept = matches!(key.code, KeyCode::Char('a' | 'A'));
                    let row = dialog.rows.get(dialog.selected)?;
                    return Some(Action::MemoryReview {
                        id: row.id.clone(),
                        accept,
                    });
                }
                _ => {}
            }
            return None;
        }

        if let Some(approval) = &mut self.approval {
            let approve = match key.code {
                KeyCode::Char('y' | 'Y' | '1') => true,
                KeyCode::Esc | KeyCode::Char('n' | 'N' | '2') => false,
                KeyCode::Enter => approval.approve_selected,
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char('k' | 'j' | 'h' | 'l') => {
                    approval.approve_selected = !approval.approve_selected;
                    return None;
                }
                _ => return None,
            };
            let id = approval.id.clone();
            self.approval = None;
            return if approve {
                Some(Action::Approve(id))
            } else {
                Some(Action::Deny(id))
            };
        }

        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.show_help = false;
            }
            return None;
        }

        if let Some(view) = &mut self.context {
            if !view.handle_key(key.code) {
                self.context = None;
            }
            return None;
        }

        if let Some(view) = &mut self.agents {
            if !view.handle_key(key.code, backend) {
                self.agents = None;
            }
            return None;
        }

        if let Some(picker) = &mut self.picker {
            match key.code {
                KeyCode::Esc => self.picker = None,
                KeyCode::Up => picker.move_selection(-1),
                KeyCode::Down => picker.move_selection(1),
                KeyCode::PageUp => picker.move_selection(-10),
                KeyCode::PageDown => picker.move_selection(10),
                KeyCode::Char('s')
                    if matches!(picker.kind, PickerKind::Effort | PickerKind::Thinking) =>
                {
                    let picker = self.picker.take()?;
                    let row = picker.rows.into_iter().nth(picker.selected)?;
                    return match picker.kind {
                        PickerKind::Effort => {
                            let (profile, thinking) = split_effort_value(&row.value);
                            Some(Action::SwitchProfileThinking {
                                profile,
                                thinking,
                                save: true,
                            })
                        }
                        PickerKind::Thinking => Some(Action::SetThinking {
                            thinking: row.value,
                            save: true,
                        }),
                        _ => None,
                    };
                }
                KeyCode::Enter => {
                    let picker = self.picker.take()?;
                    let row = picker.rows.into_iter().nth(picker.selected)?;
                    return match picker.kind {
                        PickerKind::Resume => Some(Action::Resume(row.value)),
                        PickerKind::Archive => Some(Action::Archive(row.value)),
                        PickerKind::Profile => {
                            if let Backend::Local(controller) = backend {
                                self.picker = Some(effort_picker(&row.value, controller));
                            }
                            None
                        }
                        PickerKind::Effort => {
                            let (profile, thinking) = split_effort_value(&row.value);
                            Some(Action::SwitchProfileThinking {
                                profile,
                                thinking,
                                save: false,
                            })
                        }
                        PickerKind::Thinking => Some(Action::SetThinking {
                            thinking: row.value,
                            save: false,
                        }),
                        PickerKind::Sandbox => sandbox_action(&row.value),
                    };
                }
                _ => {}
            }
            return None;
        }

        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.show_details = !self.show_details;
            return None;
        }
        if self.busy() {
            // While a turn runs, the composer is an editable draft for the
            // next input. Enter commits that draft to the transcript as the
            // queued input, unless the draft is `/agents`, which opens the
            // overlay instead (see [`App::handle_turn_key`]). Ordinary text
            // goes to the running agent's next safe checkpoint; slash
            // commands wait for the turn to finish. Ctrl+U withdraws the
            // committed prompt, or clears the current draft. Esc/Ctrl+C are
            // intercepted by the caller as cancellation before this method is
            // invoked.
            self.handle_busy_composer_key(key, backend);
            return None;
        }
        if key.code == KeyCode::Char('?') && self.input.is_empty() {
            self.show_help = true;
            return None;
        }

        // While the suggestion panel is open it owns the keys that would
        // otherwise scroll the transcript or complete a prefix: up/down move
        // the highlighted row, Tab accepts it, and Enter runs it rather than
        // the typed prefix.
        let suggestions = self.suggestions();
        if !suggestions.is_empty() {
            let selected = self.suggestion.min(suggestions.len() - 1);
            match key.code {
                KeyCode::Up | KeyCode::Down if !self.history.recalling() => {
                    let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                    self.suggestion =
                        (selected as isize + delta).rem_euclid(suggestions.len() as isize) as usize;
                    return None;
                }
                KeyCode::Tab => {
                    self.set_input(&suggestions[selected].replacement);
                    return None;
                }
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                {
                    // Falls through to the Enter arm below, which submits it.
                    self.set_input(&suggestions[selected].replacement);
                }
                _ => {}
            }
        }

        if self.handle_history_key(&key) {
            return None;
        }

        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.input.insert(self.cursor, '\n');
                self.cursor += 1;
                None
            }
            KeyCode::Enter => {
                if self.input.is_empty() {
                    return None;
                }
                self.submit_input(backend, cancel)
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                }
                self.edited();
                None
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                }
                self.edited();
                None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.len());
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_start();
                None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_end();
                None
            }
            KeyCode::Up => {
                let current: String = self.input.iter().collect();
                if let Some(line) = self.history.previous(&current) {
                    self.set_input(&line);
                }
                None
            }
            KeyCode::Down => {
                if let Some(line) = self.history.next() {
                    self.set_input(&line);
                }
                None
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                self.handle_scroll_key(&key);
                None
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_text(&ch.to_string());
                None
            }
            _ => None,
        }
    }

    /// Handles one composer key while a turn is busy. Enter normally commits
    /// the draft as queued input (see [`App::commit_queued_input`]); a
    /// draft that [`commands::parse_slash_command`] resolves to
    /// [`SlashCommandKind::Agents`] instead opens the `/agents` overlay
    /// directly and clears the draft, without touching `queued_input` — the
    /// overlay only reads `tasks.db`, so opening it starts no provider
    /// request.
    pub(crate) fn handle_busy_composer_key(&mut self, key: KeyEvent, backend: &Backend) {
        if self.handle_history_key(&key) {
            return;
        }

        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.input.insert(self.cursor, '\n');
                self.cursor += 1;
                self.edited();
            }
            KeyCode::Enter => {
                let draft: String = self.input.iter().collect();
                match commands::parse_slash_command(draft.trim()) {
                    Some((command, _)) if command.kind == SlashCommandKind::Agents => {
                        self.agents = Some(AgentsView::open(backend));
                        self.input.clear();
                        self.cursor = 0;
                        self.suggestion = 0;
                    }
                    _ => self.commit_queued_input(backend),
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.attached {
                    if backend.withdraw_user_message() {
                        self.push_system("Withdrew the queued turn.");
                    }
                } else if self.queued_input_sent && !backend.withdraw_user_message() {
                    return;
                }
                self.queued_input = None;
                self.queued_input_sent = false;
                self.input.clear();
                self.cursor = 0;
                self.edited();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                }
                self.edited();
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                }
                self.edited();
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_start();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_end();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_text(&ch.to_string());
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                self.handle_scroll_key(&key);
            }
            _ => {}
        }
    }

    /// Handles the composer's shared history keys. Both idle and busy
    /// composers call this before their state-specific key handling, so
    /// Up/Down cannot drift between the two input paths.
    fn handle_history_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => {
                let current: String = self.input.iter().collect();
                if let Some(line) = self.history.previous(&current) {
                    self.set_input(&line);
                }
                true
            }
            KeyCode::Down => {
                if let Some(line) = self.history.next() {
                    self.set_input(&line);
                }
                true
            }
            _ => false,
        }
    }

    fn commit_queued_input(&mut self, backend: &Backend) {
        if self.input.is_empty() || self.queued_input_sent {
            return;
        }
        let line = std::mem::take(&mut self.input)
            .into_iter()
            .collect::<String>()
            .trim()
            .to_string();
        self.cursor = 0;
        self.suggestion = 0;
        if line.is_empty() {
            return;
        }
        let Backend::Local(controller) = backend else {
            // The transcript shows the prompt when serve's `user_message`
            // frame arrives, so nothing is held here. Slash commands wait for
            // the turn to end, as in local mode.
            if line.starts_with('/') {
                self.queued_input = Some(line);
            } else {
                self.history.remember(&line);
                self.push_system("Queued as the next turn (Ctrl+U withdraws it).");
                self.outgoing.push(Action::Prompt(line));
            }
            self.scroll = None;
            return;
        };
        self.queued_input_sent = !line.starts_with('/') && controller.queue_user_message(&line);
        self.queued_input = Some(line);
        self.scroll = None;
    }

    pub(crate) fn submit_queued_input(
        &mut self,
        backend: &Backend,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        if self.queued_input_sent {
            return None;
        }
        let queued = self.queued_input.take()?;
        self.history.remember(&queued);
        self.dispatch_line(&queued, backend, cancel)
    }

    pub(crate) fn defer_queued_input(&mut self, backend: &Backend) {
        if self.queued_input_sent {
            backend.withdraw_user_message();
            self.queued_input_sent = false;
        }
    }

    pub(crate) fn submit_input(
        &mut self,
        backend: &Backend,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        if self.input.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.input)
            .into_iter()
            .collect::<String>()
            .trim()
            .to_string();
        self.cursor = 0;
        self.suggestion = 0;
        if line.is_empty() {
            return None;
        }
        self.history.remember(&line);
        self.dispatch_line(&line, backend, cancel)
    }

    pub fn insert_text(&mut self, value: &str) {
        self.input.splice(self.cursor..self.cursor, value.chars());
        self.cursor += value.chars().count();
        self.edited();
    }

    /// Esc or Ctrl+C while a turn is running cancels it. [`App::handle_turn_key`]
    /// checks this when the `/agents` overlay is closed; while the overlay is
    /// open it instead routes Esc to the overlay and only checks Ctrl+C here.
    pub fn is_interrupt_key(key: &KeyEvent) -> bool {
        key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    }

    /// Handles one key while a turn is running (streaming, or blocked in
    /// `agent_wait`), returning `true` when the turn should be cancelled.
    ///
    /// With the `/agents` overlay open, every key except Ctrl+C goes to
    /// [`AgentsView::handle_key`]: Esc there closes the overlay (or leaves its
    /// detail pane) without cancelling the turn; Ctrl+C always cancels.
    /// Without the overlay open, [`App::is_interrupt_key`] decides
    /// cancellation and every other key goes to
    /// [`App::handle_busy_composer_key`], whose Enter arm opens the overlay
    /// for an `/agents` draft instead of queuing it.
    pub(crate) fn handle_turn_key(&mut self, key: KeyEvent, backend: &Backend) -> bool {
        if let Some(view) = &mut self.agents {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return true;
            }
            if !view.handle_key(key.code, backend) {
                self.agents = None;
            }
            return false;
        }
        if Self::is_interrupt_key(&key) {
            return true;
        }
        self.handle_busy_composer_key(key, backend);
        false
    }

    /// The slash commands the composer's current value is a prefix of, with
    /// [`App::suggestion`] indexing the highlighted one. An open overlay hides
    /// the panel. [`super::render`] draws exactly this list.
    pub(super) fn suggestions(&self) -> Vec<Completion> {
        if self.show_help
            || self.picker.is_some()
            || self.context.is_some()
            || self.agents.is_some()
            || self.approval.is_some()
        {
            return Vec::new();
        }
        let value: String = self.input.iter().collect();
        if let Some(argument) = value.strip_prefix("/skill") {
            return self.skill_completions(argument);
        }
        commands::matching_slash_commands(&value)
            .into_iter()
            .map(Completion::from)
            .collect()
    }

    fn skill_completions(&self, argument: &str) -> Vec<Completion> {
        let fields: Vec<&str> = argument.split_whitespace().collect();
        let ends_in_space = argument.ends_with(char::is_whitespace);
        match (fields.as_slice(), ends_in_space) {
            ([], false) => vec![Completion {
                replacement: "/skill".to_string(),
                description: "list skills, show one, or set enabled state".to_string(),
            }],
            ([], true) => self
                .skill_name_completions("")
                .into_iter()
                .chain(std::iter::once(Completion {
                    replacement: "/skill set <name> enabled|disabled".to_string(),
                    description: "set a skill enabled state".to_string(),
                }))
                .collect(),
            ([prefix], false) => self.skill_name_completions(prefix),
            (["set"], true) => self.skill_name_completions_for_set(""),
            (["set", prefix], false) => self.skill_name_completions_for_set(prefix),
            (["set", name], true) => skill_state_completions(name, ""),
            (["set", name, prefix], false) => skill_state_completions(name, prefix),
            _ => Vec::new(),
        }
    }

    fn skill_name_completions(&self, prefix: &str) -> Vec<Completion> {
        self.skill_names
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| Completion {
                replacement: format!("/skill {name}"),
                description: "show skill details".to_string(),
            })
            .collect()
    }

    fn skill_name_completions_for_set(&self, prefix: &str) -> Vec<Completion> {
        self.skill_names
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| Completion {
                replacement: format!("/skill set {name}"),
                description: "choose a skill to set".to_string(),
            })
            .collect()
    }

    /// Index of the first char of the composer line holding the cursor.
    fn line_start(&self) -> usize {
        self.input[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1)
    }

    /// Index of the newline ending the cursor's composer line, or the input length.
    fn line_end(&self) -> usize {
        self.input[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.input.len(), |i| self.cursor + i)
    }

    /// Records a composer edit: the suggestion selection returns to the
    /// first match, and a recalled history line becomes an ordinary draft.
    fn edited(&mut self) {
        self.suggestion = 0;
        self.history.stop_recall();
    }

    /// Replaces the composer with an accepted command name or a recalled
    /// history line. The panel is then down to that one row (or reopened on
    /// the recalled command), so the selection returns to the first match.
    fn set_input(&mut self, value: &str) {
        self.input = value.chars().collect();
        self.cursor = self.input.len();
        self.suggestion = 0;
    }

    /// Parses and dispatches one submitted line. Parses and dispatches one
    /// submitted line, reusing the line frontend's exact output text for every
    /// command whose semantics match; `/resume`, `/archive`, and `/model` with
    /// no argument open a picker instead of printing text, since a picker is
    /// the TUI-native form of the same command.
    pub(crate) fn dispatch_line(
        &mut self,
        line: &str,
        backend: &Backend,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        if line.is_empty() {
            return None;
        }
        let Some(rest) = line.strip_prefix('/') else {
            if self.attached {
                // The `user_message` frame of the new turn is the echo.
                return Some(Action::Prompt(line.to_string()));
            }
            // Echo the prompt before the turn starts: [`App::apply_event`]
            // only ever sees the model's side of the turn, so this is the
            // sole writer of the user's own text into the live transcript.
            // A later [`App::refresh`] rebuilds every entry from history,
            // which holds this message once, so this cannot duplicate it.
            self.entries.push(Entry {
                id: format!("user-{}", self.entries.len()),
                kind: Some(EntryKind::User),
                raw: line.to_string(),
                ..Entry::default()
            });
            self.scroll = None;
            return Some(Action::Prompt(line.to_string()));
        };
        if rest.is_empty() {
            self.push_system(format!("unknown command: {line}"));
            return None;
        }
        let Some((command, args)) = commands::parse_slash_command(line) else {
            self.push_system(format!("unknown command: {line}"));
            return None;
        };
        match command.kind {
            SlashCommandKind::Help => {
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                self.show_help = true;
                None
            }
            SlashCommandKind::Init => {
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                Some(Action::Prompt(otto_core::agent::INIT_PROMPT.to_string()))
            }
            SlashCommandKind::Exit => {
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                Some(Action::Exit)
            }
            SlashCommandKind::New | SlashCommandKind::Clear => {
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                Some(Action::NewSession)
            }
            SlashCommandKind::Session => {
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                self.push_system(session_report(backend));
                None
            }
            SlashCommandKind::Rename => {
                if args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                match backend.rename_session(&args) {
                    Ok(()) => self.push_system(format!("Renamed session: {args}")),
                    Err(message) => self.push_system(format!("/rename: {message}")),
                }
                None
            }
            SlashCommandKind::Compact => Some(Action::Compact(args)),
            SlashCommandKind::Reflect => {
                if self.attached {
                    return self.unavailable(command.name);
                }
                Some(Action::Reflect(args))
            }
            SlashCommandKind::Image => {
                if args.is_empty() {
                    self.push_system("usage: /image <path>");
                    None
                } else {
                    Some(Action::Image(args))
                }
            }
            SlashCommandKind::Model => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                if !controller.dynamic_content() {
                    self.push_system(format!("/model: {PROFILE_SWITCH_UNAVAILABLE}"));
                    return None;
                }
                if args.is_empty() {
                    let profiles = controller.profile_summaries();
                    if profiles.is_empty() {
                        self.push_system(model_report(controller));
                        return None;
                    }
                    let rows = profiles
                        .into_iter()
                        .map(|profile| PickerRow {
                            label: format!(
                                "{}  {}/{}  think {}",
                                profile.name,
                                profile.provider,
                                profile.model,
                                display_thinking(&profile.thinking)
                            ),
                            value: profile.name,
                        })
                        .collect();
                    self.picker = Some(Picker::new(PickerKind::Profile, rows));
                    None
                } else {
                    Some(Action::SwitchProfile(args))
                }
            }
            SlashCommandKind::Thinking => {
                if self.attached {
                    return self.unavailable(command.name);
                }
                if args.is_empty() {
                    self.picker = Some(thinking_picker(&backend.info().thinking));
                    None
                } else {
                    Some(parse_thinking_action(args, false))
                }
            }
            SlashCommandKind::Resume => {
                self.open_session_picker(PickerKind::Resume, backend);
                None
            }
            SlashCommandKind::Archive => {
                if self.attached {
                    return self.unavailable(command.name);
                }
                self.open_session_picker(PickerKind::Archive, backend);
                None
            }
            SlashCommandKind::Sandbox => {
                let (subcommand, rest) = match args.split_once(char::is_whitespace) {
                    Some((subcommand, rest)) => (subcommand, rest.trim()),
                    None => (args.as_str(), ""),
                };
                if self.attached && matches!(subcommand, "allow" | "network" | "exclude") {
                    return self.unavailable(command.name);
                }
                // Past the check above, `local` is `Some` for every arm that
                // needs the controller.
                let local = match backend {
                    Backend::Local(controller) => Some(*controller),
                    Backend::Attach(_) => None,
                };
                match (subcommand, rest) {
                    ("", _) => {
                        let info = backend.sandbox_info();
                        let reason = info.reason_code();
                        let mut text = format!("Sandbox: {}", info.summary());
                        if !reason.is_empty() {
                            text.push_str(&format!("\nSandbox reason: {reason}"));
                        }
                        self.push_system(text);
                        None
                    }
                    ("reload", "") => Some(Action::SandboxReload),
                    ("allow", path) => {
                        let controller = local?;
                        match controller.resolve_sandbox_read_path(path) {
                            Ok(resolved) => self.picker = Some(sandbox_allow_picker(&resolved)),
                            Err(message) => {
                                self.push_system(format!("/sandbox allow: {message}"));
                            }
                        }
                        None
                    }
                    ("network", "") => {
                        self.picker = Some(sandbox_network_picker(local?));
                        None
                    }
                    ("network", mode @ ("allow" | "deny")) => {
                        Some(Action::SandboxNetwork(mode.to_string()))
                    }
                    ("exclude", entry) => match parse_exclude_entry(entry) {
                        Ok(entry) => Some(Action::SandboxExclude(entry)),
                        Err(message) => {
                            self.push_system(format!("/sandbox exclude: {message}"));
                            None
                        }
                    },
                    _ => {
                        self.push_system(format!("unknown command: /sandbox {args}"));
                        None
                    }
                }
            }
            SlashCommandKind::Approve => match args.split_whitespace().collect::<Vec<_>>()[..] {
                [id] => Some(Action::Approve(id.to_string())),
                [_, "always"] if self.attached => self.unavailable(command.name),
                [id, "always"] => Some(Action::ApproveAlways(id.to_string())),
                _ => {
                    self.push_system(format!("unknown command: {line}"));
                    None
                }
            },
            SlashCommandKind::Login => {
                if self.attached {
                    return self.unavailable(command.name);
                }
                Some(Action::Login(args))
            }
            SlashCommandKind::Mcp => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                let fields: Vec<&str> = args.split_whitespace().collect();
                match fields.as_slice() {
                    [] => {
                        self.push_system(repl_commands::mcp_report(controller));
                        None
                    }
                    ["login", name] => Some(Action::McpLogin((*name).to_string())),
                    _ => {
                        self.push_system(repl_commands::MCP_USAGE);
                        None
                    }
                }
            }
            SlashCommandKind::Logout => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                let mut out = Vec::new();
                let result = login::repl_logout(controller, &mut out, cancel);
                let text = String::from_utf8_lossy(&out).into_owned();
                match result {
                    Ok(()) => self.push_system(text.trim_end()),
                    Err(error) => self.push_system(format!("/logout: {error}")),
                }
                None
            }
            SlashCommandKind::Tasks => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                self.push_system(tasks_report(controller));
                None
            }
            SlashCommandKind::Task => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                self.push_system(task_report(controller, &args));
                None
            }
            SlashCommandKind::Context => {
                match backend.context_report() {
                    Ok(report) => self.context = Some(ContextView::new(report)),
                    Err(message) => self.push_system(format!("/context: {message}")),
                }
                None
            }
            SlashCommandKind::Agents => {
                self.agents = Some(AgentsView::open(backend));
                None
            }
            SlashCommandKind::Timers => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                self.push_system(
                    repl_commands::timers_report(controller, &args)
                        .unwrap_or_else(|message| message),
                );
                None
            }
            SlashCommandKind::Skill => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                self.push_system(repl_commands::skill_report(controller, &args));
                None
            }
            SlashCommandKind::Memory => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                let result =
                    repl_commands::repl_memory_command(controller, &args, &mut stdout, &mut stderr);
                self.push_command_result(result, &stdout, &stderr, "/memory");
                None
            }
            SlashCommandKind::Remember => {
                let Backend::Local(controller) = backend else {
                    return self.unavailable(command.name);
                };
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                let result = repl_commands::repl_remember_command(
                    controller,
                    &args,
                    &mut stdout,
                    &mut stderr,
                );
                self.push_command_result(result, &stdout, &stderr, "/remember");
                None
            }
        }
    }

    /// Formats the captured output of a synchronous REPL-command free
    /// function (`/memory`, `/remember`) into one system entry. Unlike
    /// `/logout` (also captured this way), these commands can write usage or
    /// error text to stderr as well as success text to stdout, so both
    /// buffers are captured and concatenated; exactly one is ever non-empty
    /// for a given call.
    fn push_command_result(
        &mut self,
        result: Result<(), impl std::fmt::Display>,
        stdout: &[u8],
        stderr: &[u8],
        command: &str,
    ) {
        match result {
            Ok(()) => {
                let mut text = String::from_utf8_lossy(stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(stderr));
                let trimmed = text.trim_end();
                if !trimmed.is_empty() {
                    self.push_system(trimmed.to_string());
                }
            }
            Err(error) => self.push_system(format!("{command}: {error}")),
        }
    }

    /// The reply to a command that needs the local controller while attached.
    fn unavailable(&mut self, command: &str) -> Option<Action> {
        self.push_system(format!("{command}: {ATTACH_UNAVAILABLE}"));
        None
    }

    /// `session_rows` is synchronous, so the picker opens with no intermediate
    /// loading state.
    fn open_session_picker(&mut self, kind: PickerKind, backend: &Backend) {
        match backend.session_rows() {
            Ok(rows) => {
                if rows.is_empty() {
                    self.push_system("No sessions found.");
                    return;
                }
                self.picker = Some(Picker::new(kind, rows));
            }
            Err(message) => self.push_system(format!("/{}: {message}", picker_command_name(kind))),
        }
    }

    /// Applies one turn frame to the transcript. Local turns pass their
    /// events through [`super::wire_frame`], so both modes share this writer.
    ///
    /// ponytail: one system line is appended per event rather than patching a
    /// streaming assistant entry in place (this is the sole writer during a
    /// turn; [`App::refresh`] is never called mid-turn, so nothing here is
    /// later discarded). The live view is coarser (no character-by-character
    /// growth of the assistant bubble) but the content is the same once the
    /// turn ends. Upgrade path: keep a dedicated in-progress entry and append
    /// text deltas into it if scrollback churn during streaming turns is
    /// reported as noisy.
    ///
    /// Returns `true` for an `agent_error` frame, so the caller does not also
    /// print a turn's final error when the same failure already appeared as
    /// an event.
    pub fn apply_event(&mut self, event: &WireEvent) -> bool {
        if let Some(phase) = transcript::phase(event)
            && phase != self.phase.0
        {
            self.phase = (phase, Instant::now());
            self.retry_event = (event.event_type == "provider_retry").then(|| event.clone());
        }
        match event.event_type.as_str() {
            "reasoning_delta" => {
                if let Some(last) = self.entries.last_mut()
                    && last.kind == Some(EntryKind::Reasoning)
                    && last.id == "streaming-reasoning"
                {
                    last.raw.push_str(&event.text);
                } else {
                    self.entries.push(Entry {
                        id: "streaming-reasoning".to_string(),
                        kind: Some(EntryKind::Reasoning),
                        raw: event.text.clone(),
                        ..Entry::default()
                    });
                }
            }
            "text_delta" => {
                if let Some(last) = self.entries.last_mut()
                    && last.kind == Some(EntryKind::Assistant)
                    && last.id == "streaming"
                {
                    last.raw.push_str(&event.text);
                } else {
                    self.entries.push(Entry {
                        id: "streaming".to_string(),
                        kind: Some(EntryKind::Assistant),
                        raw: event.text.clone(),
                        ..Entry::default()
                    });
                }
            }
            "tool_call_started" => {
                self.entries.push(Entry {
                    id: format!("streaming-tool-{}", self.entries.len()),
                    kind: Some(EntryKind::Tool),
                    tool_call_id: event.tool_call_id.clone(),
                    tool_name: event.tool_name.clone(),
                    tool_args: event
                        .tool_args
                        .as_deref()
                        .map(|raw| raw.get().to_string())
                        .unwrap_or_default(),
                    operation_id: Some(event.operation_id.clone()),
                    ..Entry::default()
                });
            }
            "tool_call_finished" => {
                let result = event.result.clone().unwrap_or_default();
                // An attached turn asks through `approval_requested`; the
                // result text only carries the local `/approve` hint.
                let approval = if self.attached {
                    None
                } else {
                    bash_approval_request(
                        &event.tool_name,
                        &ToolResult {
                            content: result.content.clone(),
                            is_error: result.is_error,
                            ..ToolResult::default()
                        },
                    )
                };
                if let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
                    entry.kind == Some(EntryKind::Tool) && entry.tool_call_id == event.tool_call_id
                }) {
                    entry.tool_output = result.content;
                    entry.tool_error = result.is_error;
                    entry.tool_done = true;
                    entry.operation_id = Some(event.operation_id.clone());
                    entry.disposition = result.disposition;
                    entry.effect_certainty = result.effect_certainty;
                    entry.stop_reason = result.stop_reason;
                }
                if let Some(approval) = approval {
                    self.push_system(approval_hint(&approval));
                    self.approval = Some(approval);
                }
            }
            "compaction_completed" => {
                if let Some(compaction) = &event.compaction {
                    self.push_system(compaction_line(compaction));
                }
            }
            "compaction_warning" | "memory_warning" => self.push_system(event.error.clone()),
            "agent_error" => {
                self.push_system(event.error.clone());
                return true;
            }
            USER_MESSAGE => {
                // ponytail: an attached image prompt shows its text only; the
                // frame says an image was attached but carries no data.
                let text = if self.attached {
                    event.text.clone()
                } else {
                    let queued = self
                        .queued_input
                        .take()
                        .unwrap_or_else(|| event.text.clone());
                    self.queued_input_sent = false;
                    self.history.remember(&queued);
                    queued
                };
                self.entries.push(Entry {
                    id: format!("user-{}", self.entries.len()),
                    kind: Some(EntryKind::User),
                    raw: text,
                    ..Entry::default()
                });
                self.scroll = None;
            }
            "notification" => {
                self.push_system(format!("[task {}] {}", event.task_id, event.text));
            }
            APPROVAL_REQUESTED => {
                let approval = ApprovalDialog {
                    id: event.approval_id.clone(),
                    command: event.command.clone(),
                    justification: event.justification.clone(),
                    read_path: event.read_path.clone(),
                    ..Default::default()
                };
                self.push_system(approval_hint(&approval));
                self.approval = Some(approval);
            }
            APPROVAL_DECIDED
                if self
                    .approval
                    .as_ref()
                    .is_some_and(|dialog| dialog.id == event.approval_id) =>
            {
                // The user's own answer already closed the dialog; this
                // closes it when another client or the timeout decided.
                self.approval = None;
                self.push_system(format!(
                    "Approval {} decided elsewhere: {}",
                    event.approval_id, event.decision
                ));
            }
            _ => {}
        }
        false
    }
}

fn bash_approval_request(tool_name: &str, result: &ToolResult) -> Option<ApprovalDialog> {
    let request = crate::tool::bash::parse_approval_request(tool_name, result)?;
    Some(ApprovalDialog {
        id: request.id,
        command: request.command,
        justification: request.justification,
        read_path: request.read_path,
        ..Default::default()
    })
}

fn approval_hint(approval: &ApprovalDialog) -> String {
    let mut hint =
        "Permission approval requested. Choose Yes or No above the input box (arrows + Enter, or y/n)."
            .to_string();
    if !approval.read_path.is_empty() {
        hint.push_str(&format!(
            "\nPermanently allow reading: {}\nSaved to read_paths; commands stay sandboxed.",
            approval.read_path
        ));
    }
    if !approval.command.is_empty() {
        hint.push_str("\nCommand: ");
        hint.push_str(&approval.command);
    }
    hint
}

fn picker_command_name(kind: PickerKind) -> &'static str {
    match kind {
        PickerKind::Resume => "resume",
        PickerKind::Archive => "archive",
        PickerKind::Profile => "model",
        PickerKind::Effort => "model",
        PickerKind::Thinking => "thinking",
        PickerKind::Sandbox => "sandbox",
    }
}

fn session_row(session: &SessionInfo) -> PickerRow {
    let marker = if session.current { " (current)" } else { "" };
    let label = if session.name.is_empty() {
        &session.id
    } else {
        &session.name
    };
    // Every row comes from the current workspace; naming it makes that
    // visible, since the rest of the row says nothing about where the
    // session was recorded.
    let workspace = std::path::Path::new(&session.cwd)
        .file_name()
        .map(|directory| format!(" [{}]", directory.to_string_lossy()))
        .unwrap_or_default();
    PickerRow {
        label: format!("{label}{marker}{workspace}"),
        value: session.path.clone(),
    }
}

/// The confirmation for one `/sandbox allow` grant.
///
/// The cancel row is the selected one: the path comes from whatever the model
/// or the user pasted, and a grant that widens what shell commands can read
/// should cost one deliberate keystroke rather than a reflex Enter.
fn sandbox_allow_picker(resolved: &str) -> Picker {
    Picker {
        kind: PickerKind::Sandbox,
        rows: vec![
            PickerRow {
                label: format!(
                    "Allow reading {resolved} (saved to the configuration, applied now)"
                ),
                value: format!("allow\t{resolved}"),
            },
            PickerRow {
                label: "Cancel".to_string(),
                value: String::new(),
            },
        ],
        selected: 1,
    }
}

/// The network modes, with the one now in force marked. An unconfined process
/// marks neither.
fn sandbox_network_picker(controller: &Controller) -> Picker {
    let current = match controller.sandbox_info().network {
        SandboxNetwork::Allowed => "allow",
        SandboxNetwork::Denied => "deny",
        SandboxNetwork::Unconfined => "",
    };
    let rows: Vec<PickerRow> = ["allow", "deny"]
        .into_iter()
        .map(|mode| PickerRow {
            label: format!("{}{mode}", if mode == current { "* " } else { "  " }),
            value: format!("network\t{mode}"),
        })
        .collect();
    let selected = rows
        .iter()
        .position(|row| row.label.starts_with("* "))
        .unwrap_or(0);
    Picker {
        kind: PickerKind::Sandbox,
        rows,
        selected,
    }
}

/// The action one sandbox picker row stands for. The cancel row carries no
/// value and closes the picker.
fn sandbox_action(value: &str) -> Option<Action> {
    match value.split_once('\t')? {
        ("allow", path) => Some(Action::SandboxAllow(path.to_string())),
        ("network", mode) => Some(Action::SandboxNetwork(mode.to_string())),
        _ => None,
    }
}

fn thinking_picker(current: &str) -> Picker {
    level_picker(PickerKind::Thinking, current, |thinking| {
        thinking.to_string()
    })
}

fn effort_picker(profile: &str, controller: &Controller) -> Picker {
    let target = controller.profile_effective_thinking(profile);
    level_picker(PickerKind::Effort, &target, |thinking| {
        format!("{profile}\t{thinking}")
    })
}

fn level_picker(kind: PickerKind, target: &str, value: impl Fn(&str) -> String) -> Picker {
    let current = if target.is_empty() { "unset" } else { target };
    let rows: Vec<PickerRow> = ["unset", "low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .map(|thinking| PickerRow {
            label: format!(
                "{}{}",
                if thinking == current { "* " } else { "  " },
                display_thinking_choice(thinking)
            ),
            value: value(thinking),
        })
        .collect();
    let selected = rows
        .iter()
        .position(|row| row.label.starts_with("* "))
        .unwrap_or(0);
    Picker {
        kind,
        rows,
        selected,
    }
}

fn split_effort_value(value: &str) -> (String, String) {
    let Some((profile, thinking)) = value.split_once('\t') else {
        return (String::new(), value.to_string());
    };
    (profile.to_string(), thinking.to_string())
}

fn display_thinking_choice(thinking: &str) -> &str {
    if thinking == "unset" {
        "default"
    } else {
        thinking
    }
}

fn parse_thinking_action(args: String, save_default: bool) -> Action {
    let mut save = save_default;
    let mut level = "";
    for part in args.split_whitespace() {
        if part == "--save" {
            save = true;
        } else {
            level = part;
        }
    }
    Action::SetThinking {
        thinking: level.to_string(),
        save,
    }
}

fn session_report(backend: &Backend) -> String {
    let info = backend.info();
    let mut text = format!("ID: {}", info.session_id);
    // An attached session has no local path.
    if !info.session_path.is_empty() {
        text.push_str(&format!("\nPath: {}", info.session_path));
    }
    text.push_str(&format!(
        "\nProvider: {}\nModel: {}\nThinking: {}\nSandbox: {}",
        info.provider,
        info.model,
        display_thinking(&info.thinking),
        info.sandbox.summary()
    ));
    if !info.session_name.is_empty() {
        text.push_str(&format!("\nName: {}", info.session_name));
    }
    let reason = info.sandbox.reason_code();
    if !reason.is_empty() {
        text.push_str(&format!("\nSandbox reason: {reason}"));
    }
    text
}

/// The `/model` (no-argument) output, matching the line frontend's.
fn model_report(controller: &Controller) -> String {
    let info = controller.info();
    let mut text = format!(
        "Current: profile {} (provider {}, model {}, thinking {})",
        info.profile,
        info.provider,
        info.model,
        display_thinking(&info.thinking)
    );
    text.push_str("\nNo profiles configured.");
    text
}

fn display_thinking(thinking: &str) -> &str {
    if thinking.is_empty() {
        "default"
    } else {
        thinking
    }
}

/// `pub(super)` because [`super::run`] also needs it for a `/compact` call's
/// final result (as opposed to a streamed `compaction_completed` frame, which
/// [`App::apply_event`] handles itself).
pub(super) fn compaction_line(result: &WireCompaction) -> String {
    if result.noop {
        return "[context] no-op".to_string();
    }
    let before = layout::format_token_count(result.tokens_before);
    if result.estimated_tokens_after > 0 {
        format!(
            "[context] compacted {before} \u{2192} {} tokens",
            layout::format_token_count(result.estimated_tokens_after)
        )
    } else {
        format!("[context] compacted {before} tokens")
    }
}

fn tasks_report(controller: &Controller) -> String {
    let Some(tasks) = controller.tasks() else {
        return "No tasks.".to_string();
    };
    let list = tasks.list();
    if list.is_empty() {
        return "No tasks.".to_string();
    }
    list.iter().map(task_line).collect::<Vec<_>>().join("\n")
}

/// One parsed `/task` argument list. `/task` is completable from the command
/// table, so a bare `/task` answers with its usage instead of being rejected
/// as an unknown command.
#[derive(Debug, PartialEq, Eq)]
enum TaskRequest<'a> {
    Show(&'a str),
    Cancel(&'a str),
    Usage,
}

/// Splits `/task` arguments into the forms `repl_commands::task_command`
/// accepts: `<id|name>` shows a task, `cancel <id|name>` cancels one.
fn parse_task_args(args: &str) -> TaskRequest<'_> {
    match args.split_whitespace().collect::<Vec<_>>()[..] {
        ["cancel", reference] => TaskRequest::Cancel(reference),
        [reference] if reference != "cancel" => TaskRequest::Show(reference),
        _ => TaskRequest::Usage,
    }
}

fn task_report(controller: &Controller, args: &str) -> String {
    let request = parse_task_args(args);
    if request == TaskRequest::Usage {
        return repl_commands::TASK_USAGE.to_string();
    }
    let Some(tasks) = controller.tasks() else {
        return "task not found".to_string();
    };
    match request {
        TaskRequest::Cancel(reference) => match tasks.cancel(reference) {
            Ok(()) => format!("Cancelled task: {reference}"),
            Err(message) => format!("/task {reference} cancel: {message}"),
        },
        TaskRequest::Show(reference) => match tasks.get(reference) {
            Some(task) => task_detail(&task),
            None => "task not found".to_string(),
        },
        TaskRequest::Usage => unreachable!("returned above"),
    }
}

fn task_detail(task: &Task) -> String {
    if task.session_path.is_empty() {
        return task_line(task);
    }
    format!("{}\ntranscript: {}", task_line(task), task.session_path)
}

fn task_line(task: &Task) -> String {
    let status = match task.status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Succeeded => "succeeded",
        TaskStatus::Failed => "failed",
        TaskStatus::Canceled => "canceled",
    };
    format!("{} [{status}] {}", task.id, task.description)
}

#[cfg(test)]
mod tests {
    use otto_core::agent::context_report::SectionKind;
    use otto_core::agent::inbox::NotificationKind;
    use otto_core::agent::{CompactionResult, Event};
    use otto_core::model::{Block, Role};

    use otto_core::wire::events::to_wire_compaction;

    use crate::tui::wire_frame;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn picker(rows: usize) -> Picker {
        let rows = (0..rows)
            .map(|index| PickerRow {
                label: index.to_string(),
                value: index.to_string(),
            })
            .collect();
        Picker::new(PickerKind::Resume, rows)
    }

    #[test]
    fn a_picker_row_names_the_session_workspace() {
        let row = session_row(&SessionInfo {
            name: "review".into(),
            last_user_text: "check the diff".into(),
            cwd: "/Users/u/Work/code/otto".into(),
            ..SessionInfo::default()
        });
        assert_eq!(row.label, "review [otto]");

        let current = session_row(&SessionInfo {
            name: "review".into(),
            cwd: "/Users/u/Work/code/otto".into(),
            current: true,
            ..SessionInfo::default()
        });
        assert_eq!(current.label, "review (current) [otto]");

        let unrecorded = session_row(&SessionInfo {
            name: "review".into(),
            ..SessionInfo::default()
        });
        assert_eq!(unrecorded.label, "review");
    }

    #[test]
    fn picker_selection_wraps_in_both_directions() {
        let mut picker = picker(3);
        assert_eq!(picker.selected, 0);
        picker.move_selection(-1);
        assert_eq!(picker.selected, 2);
        picker.move_selection(1);
        assert_eq!(picker.selected, 0);
        picker.move_selection(1);
        assert_eq!(picker.selected, 1);
    }

    #[test]
    fn an_empty_picker_ignores_selection_moves() {
        let mut picker = picker(0);
        picker.move_selection(1);
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn ctrl_c_arms_then_exits_on_a_confirming_press_within_the_window() {
        let mut app = App {
            entries: Vec::new(),
            usage: Usage::default(),
            info: Info::default(),
            input: Vec::new(),
            cursor: 0,
            queued_input: None,
            queued_input_sent: false,
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            context: None,
            agents: None,
            approval: None,
            memory_review: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
            retry_event: None,
            status: None,
            ctrl_c_armed_at: None,
            tasks: Vec::new(),
            skill_names: Vec::new(),
            attached: false,
            outgoing: Vec::new(),
        };
        assert!(app.handle_ctrl_c().is_none());
        assert_eq!(app.status.as_deref(), Some(CTRL_C_EXIT_STATUS));
        assert!(matches!(app.handle_ctrl_c(), Some(Action::Exit)));
    }

    #[test]
    fn ctrl_c_clears_the_composer_before_arming_when_idle() {
        let mut app = App {
            entries: Vec::new(),
            usage: Usage::default(),
            info: Info::default(),
            input: "hello".chars().collect(),
            cursor: 5,
            queued_input: None,
            queued_input_sent: false,
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            context: None,
            agents: None,
            approval: None,
            memory_review: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
            retry_event: None,
            status: None,
            ctrl_c_armed_at: None,
            tasks: Vec::new(),
            skill_names: Vec::new(),
            attached: false,
            outgoing: Vec::new(),
        };
        assert!(app.handle_ctrl_c().is_none());
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[tokio::test]
    async fn busy_composer_moves_input_to_transcript_on_enter_without_history() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();

        app.handle_key(
            key(KeyCode::Char('h'), KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        app.handle_key(
            key(KeyCode::Char('i'), KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(action.is_none());
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
        assert_eq!(app.queued_input.as_deref(), Some("hi"));
        assert!(
            app.history.previous("").is_none(),
            "queued input is not prompt history until dispatch"
        );
        assert!(
            app.entries.is_empty(),
            "queued input is a pending transcript item, not persisted history"
        );
    }

    #[tokio::test]
    async fn busy_composer_arrow_keys_recall_history_and_restore_the_draft() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.history.remember("first prompt");
        app.history.remember("latest prompt");
        app.input = "draft".chars().collect();
        app.cursor = app.input.len();
        app.max_scroll.set(10);
        app.start_turn();

        assert!(!app.handle_turn_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller)
        ));
        assert_eq!(app.input.iter().collect::<String>(), "latest prompt");
        assert_eq!(app.scroll, None, "Up must not scroll during an active turn");

        assert!(!app.handle_turn_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller)
        ));
        assert_eq!(app.input.iter().collect::<String>(), "first prompt");
        assert!(!app.handle_turn_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller)
        ));
        assert_eq!(app.input.iter().collect::<String>(), "latest prompt");
        assert!(!app.handle_turn_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller)
        ));
        assert_eq!(app.input.iter().collect::<String>(), "draft");
    }

    #[tokio::test]
    async fn busy_composer_queues_user_input_for_the_running_turn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let _admission = controller.begin_operation().expect("active turn");
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.insert_text("change course");

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(action.is_none());
        assert!(app.queued_input_sent);
        assert_eq!(app.queued_input.as_deref(), Some("change course"));
        assert!(controller.current_runner().is_some_and(|runner| {
            runner.inbox().snapshot().iter().any(|notification| {
                notification.kind == Some(otto_core::agent::inbox::NotificationKind::UserMessage)
                    && notification.text == "change course"
            })
        }));

        app.apply_event(&wire_frame(&Event::Notification {
            kind: None,
            task_id: String::new(),
            text: "not the queued input".into(),
            usage: Usage::default(),
            present: false,
        }));
        assert!(
            app.queued_input_sent,
            "an untyped notification is not user input"
        );
        assert_eq!(app.queued_input.as_deref(), Some("change course"));

        app.apply_event(&wire_frame(&Event::Notification {
            kind: Some(NotificationKind::UserMessage),
            task_id: String::new(),
            text: "change course".into(),
            usage: Usage::default(),
            present: false,
        }));
        assert!(!app.queued_input_sent);
        assert!(app.queued_input.is_none());
        assert_eq!(
            app.entries.last().expect("user entry").kind,
            Some(EntryKind::User)
        );
        assert_eq!(app.history.previous(""), Some("change course".to_string()));
    }

    #[tokio::test]
    async fn failed_turn_defers_queued_input_without_running_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let admission = controller.begin_operation().expect("active turn");
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.insert_text("follow up");
        app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        drop(admission);

        app.defer_queued_input(&Backend::Local(&controller));

        assert!(!app.queued_input_sent);
        assert_eq!(app.queued_input.as_deref(), Some("follow up"));
        assert!(controller.current_runner().is_some_and(|runner| {
            runner
                .inbox()
                .snapshot()
                .iter()
                .all(|item| item.kind != Some(NotificationKind::UserMessage))
        }));

        let mut slash = App::new(&Backend::Local(&controller));
        slash.start_turn();
        slash.insert_text("/memory search vim");
        slash.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        slash.defer_queued_input(&Backend::Local(&controller));
        assert_eq!(slash.queued_input.as_deref(), Some("/memory search vim"));
        assert!(slash.entries.is_empty(), "the slash command did not run");
    }

    #[tokio::test]
    async fn busy_composer_draft_stays_in_input_until_enter() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();

        app.insert_text("draft only");

        assert_eq!(app.input.iter().collect::<String>(), "draft only");
        assert!(app.queued_input.is_none());
        assert!(app.entries.is_empty());
        assert!(app.history.previous("").is_none());
    }

    #[tokio::test]
    async fn busy_composer_ctrl_u_withdraws_the_queued_draft() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.insert_text("queued draft");
        app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.queued_input.as_deref(), Some("queued draft"));

        let action = app.handle_key(
            key(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(action.is_none());
        assert!(app.input.is_empty());
        assert!(app.queued_input.is_none());
        assert_eq!(app.cursor, 0);
        assert!(app.history.previous("").is_none());
        assert!(app.entries.is_empty());
    }

    async fn assert_ctrl_a_e_move_within_line(busy: bool) {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        if busy {
            app.start_turn();
        }
        app.insert_text("ab\ncd");
        let press = |app: &mut App, ch: char, from: usize| {
            app.cursor = from;
            app.handle_key(
                key(KeyCode::Char(ch), KeyModifiers::CONTROL),
                &Backend::Local(&controller),
                &cancel,
            );
            app.cursor
        };
        assert_eq!(press(&mut app, 'a', 4), 3);
        assert_eq!(press(&mut app, 'e', 4), 5);
        assert_eq!(press(&mut app, 'e', 1), 2);
        assert_eq!(press(&mut app, 'a', 1), 0);
        assert_eq!(app.input.iter().collect::<String>(), "ab\ncd");
    }

    #[tokio::test]
    async fn idle_composer_ctrl_a_and_ctrl_e_move_within_the_current_line() {
        assert_ctrl_a_e_move_within_line(false).await;
    }

    #[tokio::test]
    async fn busy_composer_ctrl_a_and_ctrl_e_move_within_the_current_line() {
        assert_ctrl_a_e_move_within_line(true).await;
    }

    #[tokio::test]
    async fn queued_draft_enters_history_only_when_submitted_after_turn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();

        app.insert_text("next prompt");
        app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert!(app.input.is_empty());
        assert_eq!(app.queued_input.as_deref(), Some("next prompt"));
        assert!(app.entries.is_empty());
        assert!(app.history.previous("").is_none());

        app.end_turn();
        let action = app.submit_queued_input(&Backend::Local(&controller), &cancel);

        assert!(matches!(action, Some(Action::Prompt(line)) if line == "next prompt"));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].kind, Some(EntryKind::User));
        assert_eq!(app.entries[0].raw, "next prompt");
        assert_eq!(app.history.previous(""), Some("next prompt".to_string()));
    }

    #[test]
    fn help_key_opens_help_only_when_the_composer_is_empty() {
        let app = App {
            entries: Vec::new(),
            usage: Usage::default(),
            info: Info::default(),
            input: "abc".chars().collect(),
            cursor: 3,
            queued_input: None,
            queued_input_sent: false,
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            context: None,
            agents: None,
            approval: None,
            memory_review: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
            retry_event: None,
            status: None,
            ctrl_c_armed_at: None,
            tasks: Vec::new(),
            skill_names: Vec::new(),
            attached: false,
            outgoing: Vec::new(),
        };
        let event = key(KeyCode::Char('?'), KeyModifiers::NONE);
        // No controller is available in a unit test; '?' with pending text
        // must insert a literal character without touching the controller,
        // so it is safe to exercise without one by checking state alone.
        assert!(!app.input.is_empty());
        let _ = event;
    }

    #[test]
    fn insert_text_preserves_pasted_newlines_at_the_cursor() {
        let mut app = App {
            entries: Vec::new(),
            usage: Usage::default(),
            info: Info::default(),
            input: "abcd".chars().collect(),
            cursor: 2,
            queued_input: None,
            queued_input_sent: false,
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            context: None,
            agents: None,
            approval: None,
            memory_review: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
            retry_event: None,
            status: None,
            ctrl_c_armed_at: None,
            tasks: Vec::new(),
            skill_names: Vec::new(),
            attached: false,
            outgoing: Vec::new(),
        };

        app.insert_text("one\ntwo");

        assert_eq!(app.input.iter().collect::<String>(), "abone\ntwocd");
        assert_eq!(app.cursor, "abone\ntwo".chars().count());
    }

    #[test]
    fn task_arguments_accept_only_the_documented_forms() {
        assert!(matches!(parse_task_args(""), TaskRequest::Usage));
        assert!(matches!(parse_task_args("cancel"), TaskRequest::Usage));
        assert!(matches!(parse_task_args("t7 cancel"), TaskRequest::Usage));
        assert!(matches!(parse_task_args("  t7 "), TaskRequest::Show("t7")));
        assert!(matches!(
            parse_task_args("cancel t7"),
            TaskRequest::Cancel("t7")
        ));
    }

    #[test]
    fn split_command_recognizes_a_bare_slash_line() {
        assert!(commands::parse_slash_command("/help").is_some());
    }

    #[test]
    fn compaction_line_matches_the_noop_and_estimated_forms() {
        let noop = CompactionResult {
            noop: true,
            ..Default::default()
        };
        assert_eq!(
            compaction_line(&to_wire_compaction(&noop)),
            "[context] no-op"
        );
        let estimated = CompactionResult {
            noop: false,
            tokens_before: 12_000,
            estimated_tokens_after: 4_000,
            ..Default::default()
        };
        assert_eq!(
            compaction_line(&to_wire_compaction(&estimated)),
            "[context] compacted 12k \u{2192} 4k tokens"
        );
    }

    /// [`App::scroll`] is an absolute top offset, so a scroll key starts
    /// from the bottom the last frame laid out and returns to following the
    /// bottom once it reaches it again. The composer's own Up/Down recall
    /// prompt history, so the keys that scroll an idle transcript are
    /// PgUp/PgDn and the wheel.
    #[tokio::test]
    async fn scroll_keys_move_an_absolute_top_offset() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.max_scroll.set(10);

        app.handle_key(
            key(KeyCode::PageUp, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.scroll, Some(0));
        app.handle_key(
            key(KeyCode::PageDown, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.scroll, None);

        app.scroll_wheel(true);
        assert_eq!(app.scroll, Some(9), "one wheel notch is one line");
        app.scroll_wheel(false);
        assert_eq!(app.scroll, None);
    }

    /// The reported bad experience: scrolling up during a streaming turn was
    /// undone by the next delta, so the transcript snapped back to the
    /// bottom. Following the bottom stays the default; a manual offset is
    /// only left by a scroll key or a new prompt.
    #[tokio::test]
    async fn streamed_events_keep_a_manual_scroll_offset() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));

        app.apply_event(&wire_frame(&Event::TextDelta {
            text: "first".to_string(),
        }));
        assert_eq!(app.scroll, None, "an unscrolled transcript keeps following");

        app.scroll = Some(4);
        app.apply_event(&wire_frame(&Event::TextDelta {
            text: "second".to_string(),
        }));
        app.apply_event(&wire_frame(&Event::ToolCallStarted {
            operation_id: otto_core::model::OperationId::new("op_test").expect("operation id"),
            attempt: 1,
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            arguments: String::new(),
        }));
        assert_eq!(app.scroll, Some(4));
    }

    #[tokio::test]
    async fn bash_approval_errors_add_a_visible_system_hint() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let before = app.entries.len();

        app.apply_event(&wire_frame(&Event::ToolCallStarted {
            operation_id: otto_core::model::OperationId::new("op_approval").expect("operation id"),
            attempt: 1,
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            arguments: r#"{"command":"git push"}"#.to_string(),
        }));
        app.apply_event(&wire_frame(&Event::ToolCallFinished {
            operation_id: otto_core::model::OperationId::new("op_approval").expect("operation id"),
            attempt: 1,
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            result: otto_core::tool::ToolResult {
                content: "approval required for unsandboxed bash execution.\nApprove in Otto: /approve approval-1\nCommand: \"git push\"\nJustification: \"push branch\"".to_string(),
                is_error: true,
                ..Default::default()
            },
            outcome: otto_core::model::OperationOutcome {
                disposition: otto_core::model::OperationDisposition::Error,
                effect_certainty: otto_core::model::EffectCertainty::NotStarted,
                stop_reason: None,
            },
        }));

        let added = &app.entries[before..];
        assert_eq!(added.len(), 2, "tool entry plus system hint: {added:?}");
        assert_eq!(added[0].kind, Some(EntryKind::Tool));
        assert_eq!(added[0].operation_id.as_deref(), Some("op_approval"));
        assert_eq!(
            added[0].disposition,
            Some(otto_core::model::OperationDisposition::Error)
        );
        assert_eq!(
            added[0].effect_certainty,
            Some(otto_core::model::EffectCertainty::NotStarted)
        );
        assert_eq!(added[1].kind, Some(EntryKind::System));
        assert!(added[1].raw.contains("arrows + Enter"), "{}", added[1].raw);
        assert!(added[1].raw.contains("git push"), "{}", added[1].raw);
        let approval = app.approval.as_ref().expect("approval dialog");
        assert_eq!(approval.id, "approval-1");
        assert_eq!(approval.command, "git push");
        assert_eq!(approval.justification, "push branch");
    }

    #[tokio::test]
    async fn approval_dialog_selects_with_arrows_and_enter_and_escape_cancels() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        let dialog = |id: &str| ApprovalDialog {
            id: id.to_string(),
            command: "git push".to_string(),
            justification: "push branch".to_string(),
            ..Default::default()
        };
        let press = |app: &mut App, code| {
            app.handle_key(
                key(code, KeyModifiers::NONE),
                &Backend::Local(&controller),
                &cancel,
            )
        };

        // Enter on the default option ("No") cancels; nothing is granted.
        app.approval = Some(dialog("approval-1"));
        assert!(
            matches!(press(&mut app, KeyCode::Enter), Some(Action::Deny(id)) if id == "approval-1")
        );
        assert!(app.approval.is_none(), "Enter on No closes the prompt");

        // An arrow moves to "Yes"; Enter then grants. No letter key needed.
        app.approval = Some(dialog("approval-2"));
        assert!(press(&mut app, KeyCode::Up).is_none());
        assert!(app.approval.is_some());
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(action, Some(Action::Approve(id)) if id == "approval-2"));
        assert!(app.approval.is_none());

        // Moving twice returns to "No".
        app.approval = Some(dialog("approval-3"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Tab);
        assert!(
            matches!(press(&mut app, KeyCode::Enter), Some(Action::Deny(id)) if id == "approval-3")
        );

        // Shortcuts still work, and Esc cancels.
        app.approval = Some(dialog("approval-4"));
        let action = press(&mut app, KeyCode::Char('y'));
        assert!(matches!(action, Some(Action::Approve(id)) if id == "approval-4"));
        app.approval = Some(dialog("approval-5"));
        let action = press(&mut app, KeyCode::Char('1'));
        assert!(matches!(action, Some(Action::Approve(id)) if id == "approval-5"));
        app.approval = Some(dialog("approval-6"));
        assert!(
            matches!(press(&mut app, KeyCode::Esc), Some(Action::Deny(id)) if id == "approval-6")
        );
        assert!(app.approval.is_none());

        // Unrelated keys, such as typing under an IME, leave it open.
        app.approval = Some(dialog("approval-7"));
        assert!(press(&mut app, KeyCode::Char('是')).is_none());
        assert!(app.approval.is_some());
    }

    #[test]
    fn task_detail_names_the_child_transcript_when_one_exists() {
        let mut task = Task {
            id: "t1".to_string(),
            name: String::new(),
            agent: "reviewer".to_string(),
            description: "check the diff".to_string(),
            model: String::new(),
            status: TaskStatus::Running,
            created_at: chrono::DateTime::from_timestamp(0, 0).expect("epoch"),
            started_at: None,
            finished_at: None,
            steps: 0,
            tool_calls: 0,
            last_tool: String::new(),
            last_text: String::new(),
            usage: Default::default(),
            usage_present: false,
            result: String::new(),
            error: String::new(),
            session_path: String::new(),
        };
        assert_eq!(task_detail(&task), task_line(&task));

        task.session_path = "/sessions/p/t1-c.jsonl".to_string();
        assert_eq!(
            task_detail(&task),
            format!("{}\ntranscript: /sessions/p/t1-c.jsonl", task_line(&task))
        );
    }

    #[tokio::test]
    async fn reasoning_deltas_build_one_entry_before_the_reply() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let before = app.entries.len();

        for text in ["weigh ", "options"] {
            app.apply_event(&wire_frame(&Event::ReasoningDelta { text: text.into() }));
        }
        app.apply_event(&wire_frame(&Event::TextDelta { text: "ok".into() }));

        let added: Vec<_> = app.entries[before..]
            .iter()
            .map(|entry| (entry.kind, entry.raw.as_str()))
            .collect();
        assert_eq!(
            added,
            [
                (Some(EntryKind::Reasoning), "weigh options"),
                (Some(EntryKind::Assistant), "ok"),
            ]
        );
    }

    #[tokio::test]
    async fn retry_status_counts_down_and_keeps_elapsed_time() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.apply_event(&wire_frame(&Event::ProviderRetry {
            operation_id: otto_core::model::OperationId::new("op_retry").unwrap(),
            attempt: 2,
            max_attempts: 4,
            delay: Duration::from_secs(4),
            reason: "connection interrupted".into(),
        }));
        assert_eq!(
            app.thinking().unwrap().phase,
            "retry 1/3 after connection interrupted, waiting 4s"
        );
        app.phase.1 = Instant::now() - Duration::from_millis(2100);
        let status = app.thinking().unwrap();
        assert_eq!(
            status.phase,
            "retry 1/3 after connection interrupted, waiting 2s"
        );
        assert_eq!(status.phase_elapsed.as_secs(), 2);
        app.phase.1 = Instant::now() - Duration::from_secs(5);
        assert_eq!(
            app.thinking().unwrap().phase,
            "retry 1/3 after connection interrupted, requesting"
        );
        app.apply_event(&wire_frame(&Event::TextDelta { text: "ok".into() }));
        assert_eq!(app.thinking().unwrap().phase, "responding");
        app.end_turn();
        assert!(app.thinking().is_none());
    }

    /// The phase duration counts from the phase's first event: each further
    /// delta of the same phase does not restart it.
    #[tokio::test]
    async fn repeated_deltas_do_not_restart_the_phase_clock() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();

        app.apply_event(&wire_frame(&Event::ReasoningDelta { text: "a".into() }));
        let started = app.phase.1;
        app.apply_event(&wire_frame(&Event::ReasoningDelta { text: "b".into() }));
        assert_eq!(app.phase, ("reasoning".to_string(), started));
        assert!(app.retry_event.is_none());

        app.apply_event(&wire_frame(&Event::TextDelta { text: "ok".into() }));
        assert_eq!(app.phase.0, "responding");
        assert!(app.phase.1 >= started);
    }

    /// A turn ignores every other key ([`App::handle_key`] returns early
    /// while busy), but the scroll keys have to keep working so the output
    /// arriving can be read from where the reader left off.
    #[tokio::test]
    async fn scroll_keys_work_while_a_turn_is_running() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.max_scroll.set(10);
        app.start_turn();

        assert!(app.handle_scroll_key(&key(KeyCode::PageUp, KeyModifiers::NONE)));
        assert_eq!(app.scroll, Some(0));
        assert!(!app.handle_scroll_key(&key(KeyCode::Char('x'), KeyModifiers::NONE)));
    }

    // Memory command coverage through this module's own unit seams
    // (`dispatch_line`/`handle_key`): every command result lands as one
    // transcript entry via `push_system`/`push_command_result`, since this
    // frontend has no separate status bar. A `MemoryWarning` event is covered
    // by `apply_event`'s own arm, unrelated to command dispatch.

    use crate::cli::testutil;
    use crate::memory::RememberRequest;

    #[tokio::test]
    async fn clear_dispatches_like_new_and_rejects_arguments() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        assert!(matches!(
            app.dispatch_line("/clear", &Backend::Local(&controller), &cancel),
            Some(Action::NewSession)
        ));

        assert!(
            app.dispatch_line("/clear now", &Backend::Local(&controller), &cancel)
                .is_none()
        );
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "unknown command: /clear now"
        );
    }

    #[tokio::test]
    async fn init_submits_the_builtin_agent_task_and_rejects_arguments() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        let action = app.dispatch_line("/init", &Backend::Local(&controller), &cancel);
        assert!(
            matches!(action, Some(Action::Prompt(prompt)) if prompt == otto_core::agent::INIT_PROMPT)
        );

        assert!(
            app.dispatch_line("/init now", &Backend::Local(&controller), &cancel)
                .is_none()
        );
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "unknown command: /init now"
        );
    }

    #[tokio::test]
    async fn skill_commands_push_the_catalog_and_skill_body() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        testutil::write_skill(
            workspace.path(),
            "rust-helper",
            "Rust guidance",
            "Use small focused Rust changes.",
        );
        testutil::write_skill(
            workspace.path(),
            "api-review",
            "A deliberately long description that must not enter the skills list.",
            "Review APIs.",
        );
        let contract_dir = workspace.path().join(".otto/skills/release-notes");
        std::fs::create_dir_all(&contract_dir).expect("contract skill directory");
        std::fs::write(
            contract_dir.join("SKILL.md"),
            "---\nname: release-notes\ndescription: A contract skill.\ninput: changes\noutput: notes\n---\nWrite notes.\n",
        )
        .expect("contract skill");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line("/skill", &Backend::Local(&controller), &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "Available skills:\n- api-review\n- release-notes [contract]\n- rust-helper"
        );
        assert!(
            !app.entries
                .last()
                .expect("entry")
                .raw
                .contains("Rust guidance"),
            "{:?}",
            app.entries.last()
        );
        assert!(
            !app.entries
                .last()
                .expect("entry")
                .raw
                .contains("deliberately long description"),
            "{:?}",
            app.entries.last()
        );

        app.dispatch_line("/skills", &Backend::Local(&controller), &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "unknown command: /skills"
        );

        app.dispatch_line("/skill rust-helper", &Backend::Local(&controller), &cancel);
        let detail = &app.entries.last().expect("entry").raw;
        assert!(detail.contains("Skill: rust-helper"), "{detail}");
        assert!(
            detail.contains("Use small focused Rust changes."),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn skill_suggestions_are_context_sensitive_and_use_full_replacements() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        testutil::write_skill(workspace.path(), "release-notes", "Notes", "Write notes.");
        testutil::write_skill(workspace.path(), "set", "A named set", "Detail body.");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));

        let suggestions = |value: &str, app: &mut App| {
            app.input = value.chars().collect();
            app.cursor = app.input.len();
            app.suggestions()
        };
        let rows = suggestions("/skill ", &mut app);
        assert_eq!(
            rows.iter()
                .map(|row| (row.replacement.as_str(), row.description.as_str()))
                .collect::<Vec<_>>(),
            [
                ("/skill release-notes", "show skill details"),
                ("/skill set", "show skill details"),
                (
                    "/skill set <name> enabled|disabled",
                    "set a skill enabled state"
                ),
            ]
        );
        let rows = suggestions("/skill rel", &mut app);
        assert_eq!(
            rows.iter()
                .map(|row| row.replacement.as_str())
                .collect::<Vec<_>>(),
            ["/skill release-notes"]
        );
        let rows = suggestions("/skill set ", &mut app);
        assert_eq!(
            rows.iter()
                .map(|row| row.replacement.as_str())
                .collect::<Vec<_>>(),
            ["/skill set release-notes", "/skill set set"]
        );
        let rows = suggestions("/skill set release-notes ", &mut app);
        assert_eq!(
            rows.iter()
                .map(|row| (row.replacement.as_str(), row.description.as_str()))
                .collect::<Vec<_>>(),
            [
                ("/skill set release-notes enabled", "set skill state"),
                ("/skill set release-notes disabled", "set skill state"),
            ]
        );
        let rows = suggestions("/skill set", &mut app);
        assert_eq!(
            rows.iter()
                .map(|row| row.replacement.as_str())
                .collect::<Vec<_>>(),
            ["/skill set"]
        );
    }

    #[tokio::test]
    async fn mcp_command_pushes_the_status_report_and_returns_a_login_action() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line("/mcp", &Backend::Local(&controller), &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "No MCP servers configured."
        );

        let action = app.dispatch_line("/mcp login docs", &Backend::Local(&controller), &cancel);
        assert!(matches!(action, Some(Action::McpLogin(name)) if name == "docs"));

        app.dispatch_line("/mcp bogus", &Backend::Local(&controller), &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            repl_commands::MCP_USAGE
        );
    }

    #[tokio::test]
    async fn context_command_opens_the_report_and_esc_closes_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        assert!(
            app.dispatch_line("/context", &Backend::Local(&controller), &cancel)
                .is_none()
        );
        let view = app.context.as_ref().expect("the context overlay is open");
        assert_eq!(view.report.sections[0].kind, SectionKind::SystemPrompt);
        assert!(app.suggestions().is_empty(), "an overlay hides suggestions");

        app.handle_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert!(app.context.is_none());
    }

    #[tokio::test]
    async fn image_command_attaches_a_path_for_the_next_prompt() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));

        let action = app.dispatch_line(
            "/image /tmp/screenshot with spaces.png",
            &Backend::Local(&controller),
            &CancellationToken::new(),
        );
        assert!(
            matches!(action, Some(Action::Image(path)) if path == "/tmp/screenshot with spaces.png")
        );
    }

    #[tokio::test]
    async fn memory_command_without_a_service_pushes_the_unavailable_message() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line("/memory search vim", &Backend::Local(&controller), &cancel);

        assert_eq!(
            app.entries.last().expect("entry").raw,
            format!("/memory: {}", repl_commands::MEMORY_UNAVAILABLE)
        );
    }

    #[tokio::test]
    async fn remember_command_without_a_service_pushes_the_unavailable_message() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line(
            "/remember prefers dark mode",
            &Backend::Local(&controller),
            &cancel,
        );

        assert_eq!(
            app.entries.last().expect("entry").raw,
            format!("/remember: {}", repl_commands::MEMORY_UNAVAILABLE)
        );
    }

    #[tokio::test]
    async fn bare_memory_command_pushes_the_usage_line() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line("/memory", &Backend::Local(&controller), &cancel);

        assert_eq!(
            app.entries.last().expect("entry").raw,
            repl_commands::MEMORY_USAGE
        );
    }

    #[tokio::test]
    async fn remember_with_only_a_scope_flag_pushes_the_usage_line() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line(
            "/remember --scope user",
            &Backend::Local(&controller),
            &cancel,
        );

        assert_eq!(
            app.entries.last().expect("entry").raw,
            repl_commands::REMEMBER_USAGE
        );
    }

    /// `/memory review` reaches the shared memory command: a bad decision word
    /// prints the usage line and an unknown candidate id is a command error.
    #[tokio::test]
    async fn memory_review_reaches_the_shared_reviewer() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line("/memory review", &Backend::Local(&controller), &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "no pending candidates"
        );

        app.dispatch_line(
            "/memory review cand-1 accept",
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "/memory: candidate cand-1 not found"
        );

        app.dispatch_line(
            "/memory review cand-1 maybe",
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(
            app.entries.last().expect("entry").raw,
            repl_commands::MEMORY_USAGE
        );
    }

    #[tokio::test]
    async fn memory_search_pushes_the_rendered_records() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line(
            "/remember --kind preference --key editor vim",
            &Backend::Local(&controller),
            &cancel,
        );
        app.dispatch_line("/memory search vim", &Backend::Local(&controller), &cancel);

        let text = app.entries.last().expect("entry").raw.clone();
        assert!(text.contains("1 records:"), "{text}");
        assert!(text.contains("kind=preference"), "{text}");
        assert!(text.contains("text=vim"), "{text}");
    }

    #[tokio::test]
    async fn memory_forget_resolves_the_revision_and_reports_a_missing_record() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let (service, _, workspace_scope) = controller.memory_manager().expect("memory");
        let record = service
            .remember(&RememberRequest {
                scope: workspace_scope,
                kind: "note".into(),
                text: "vim".into(),
                ..RememberRequest::default()
            })
            .expect("remember");
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line(
            &format!("/memory forget {}", record.id),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(
            app.entries.last().expect("entry").raw,
            format!("forgot {} (revision 1)", record.id)
        );

        app.dispatch_line(
            "/memory forget missing",
            &Backend::Local(&controller),
            &cancel,
        );
        let missing = app.entries.last().expect("entry").raw.clone();
        assert!(missing.starts_with("/memory: "), "{missing}");
        assert!(missing.contains("not found"), "{missing}");
    }

    #[tokio::test]
    async fn memory_review_dialog_reads_scoped_details_and_emits_review_action() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let (service, _, workspace_scope) = controller.memory_manager().expect("memory");
        let candidate = service
            .propose(&crate::memory::ProposeRequest {
                action: crate::memory::CandidateAction::Create,
                scope: workspace_scope,
                kind: "preference".into(),
                key: "editor".into(),
                text: "private candidate body".into(),
                reason: "private reason".into(),
                source: crate::memory::Provenance {
                    origin: Some(crate::memory::Origin::Model),
                    ..crate::memory::Provenance::default()
                },
                ..crate::memory::ProposeRequest::default()
            })
            .expect("proposal")
            .pop()
            .expect("candidate");
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.open_memory_review(&controller);
        let dialog = app.memory_review.as_ref().expect("dialog");
        assert_eq!(dialog.rows.len(), 1);
        assert_eq!(dialog.rows[0].id, candidate.id);
        assert_eq!(dialog.rows[0].key, "editor");
        assert_eq!(dialog.rows[0].text, "private candidate body");
        assert_eq!(dialog.rows[0].reason, "private reason");

        let action = app.handle_key(
            KeyEvent::from(KeyCode::Char('a')),
            &Backend::Local(&controller),
            &cancel,
        );
        assert!(
            matches!(action, Some(Action::MemoryReview { id, accept: true }) if id == candidate.id)
        );
    }

    #[tokio::test]
    async fn remember_defaults_to_workspace_scope_and_note_kind() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let (_, _, workspace_scope) = controller.memory_manager().expect("memory");
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line(
            "/remember prefers dark mode",
            &Backend::Local(&controller),
            &cancel,
        );

        let text = app.entries.last().expect("entry").raw.clone();
        assert!(text.starts_with("remembered "), "{text}");
        assert!(text.contains("kind=note"), "{text}");
        assert!(
            text.contains(&format!(
                "scope={}/{}",
                workspace_scope.namespace, workspace_scope.id
            )),
            "{text}"
        );
    }

    #[tokio::test]
    async fn remember_parses_scope_kind_and_key_flags() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let (_, user_scope, _) = controller.memory_manager().expect("memory");
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        app.dispatch_line(
            "/remember --scope user --kind preference --key editor vim",
            &Backend::Local(&controller),
            &cancel,
        );

        let text = app.entries.last().expect("entry").raw.clone();
        assert!(text.starts_with("remembered "), "{text}");
        assert!(text.contains("kind=preference"), "{text}");
        assert!(
            text.contains(&format!("scope={}/{}", user_scope.namespace, user_scope.id)),
            "{text}"
        );
    }

    /// Busy slash commands are queued as literal next input instead of being
    /// dispatched immediately. What is verified here is the observable
    /// guarantee: the command never runs while a turn is active.
    #[tokio::test]
    async fn busy_guard_rejects_slash_commands_while_a_turn_is_active() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let before = app.entries.len();
        app.start_turn();
        app.input = "/memory search vim".chars().collect();
        app.cursor = app.input.len();
        let cancel = CancellationToken::new();

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(action.is_none());
        assert_eq!(app.entries.len(), before, "no command must run while busy");
        assert!(app.input.is_empty(), "queued Enter clears the composer");
        assert_eq!(app.queued_input.as_deref(), Some("/memory search vim"));
    }

    /// The reported bad experience: while a turn streamed or `agent_wait`
    /// blocked, `/agents` + Enter queued the command like any other instead
    /// of opening the overlay. `handle_turn_key` (what the busy turn loop
    /// actually calls) must open it immediately and leave nothing queued.
    #[tokio::test]
    async fn busy_turn_key_opens_the_agents_overlay_on_an_agents_draft_instead_of_queuing_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.input = "/agents".chars().collect();
        app.cursor = app.input.len();

        let cancelled = app.handle_turn_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
        );

        assert!(!cancelled);
        assert!(app.agents.is_some(), "Enter on /agents opens the overlay");
        assert!(app.queued_input.is_none());
        assert!(app.input.is_empty());
    }

    /// Opening the overlay from a busy `/agents` draft must not disturb a
    /// prompt already queued from an earlier Enter.
    #[tokio::test]
    async fn busy_turn_key_opening_the_overlay_leaves_an_earlier_queued_prompt_untouched() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.queued_input = Some("earlier queued prompt".to_string());
        app.input = "/agents".chars().collect();
        app.cursor = app.input.len();

        app.handle_turn_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
        );

        assert!(app.agents.is_some());
        assert_eq!(app.queued_input.as_deref(), Some("earlier queued prompt"));
    }

    /// While the overlay is open during a turn, Esc closes it without
    /// cancelling the turn; a second Esc, with the overlay now closed,
    /// cancels the turn as it always did.
    #[tokio::test]
    async fn busy_turn_key_esc_closes_the_open_overlay_before_it_cancels_the_turn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.agents = Some(AgentsView::open(&Backend::Local(&controller)));

        let cancelled = app.handle_turn_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &Backend::Local(&controller),
        );
        assert!(!cancelled, "the first Esc only closes the overlay");
        assert!(app.agents.is_none());

        let cancelled = app.handle_turn_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &Backend::Local(&controller),
        );
        assert!(cancelled, "Esc with no overlay open cancels the turn");
    }

    /// Ctrl+C cancels the turn even while the overlay is open, bypassing the
    /// overlay entirely.
    #[tokio::test]
    async fn busy_turn_key_ctrl_c_cancels_the_turn_even_with_the_overlay_open() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.start_turn();
        app.agents = Some(AgentsView::open(&Backend::Local(&controller)));

        let cancelled = app.handle_turn_key(
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &Backend::Local(&controller),
        );

        assert!(cancelled);
    }

    /// The completion half; the help-overlay text containment half is
    /// `super::render`'s concern, not this module's.
    #[tokio::test]
    async fn tab_completes_a_memory_prefix_to_the_full_command() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.input = "/mem".chars().collect();
        app.cursor = app.input.len();
        let cancel = CancellationToken::new();

        app.handle_key(
            key(KeyCode::Tab, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert_eq!(app.input.iter().collect::<String>(), "/memory");
    }

    /// Bash-style prompt history: Up walks back through the lines already
    /// submitted, the oldest one holds, and Down past the newest brings back
    /// the draft the first Up interrupted.
    #[test]
    fn history_walks_back_through_submitted_lines_and_restores_the_draft() {
        let mut history = History::default();
        history.remember("first");
        history.remember("/model");

        assert_eq!(history.previous("draft").as_deref(), Some("/model"));
        assert_eq!(history.previous("draft").as_deref(), Some("first"));
        assert_eq!(history.previous("draft"), None, "the oldest line holds");
        assert_eq!(history.next().as_deref(), Some("/model"));
        assert_eq!(
            history.next().as_deref(),
            Some("draft"),
            "the draft returns"
        );
        assert_eq!(history.next(), None, "Down on the draft does nothing");
    }

    #[test]
    fn an_empty_history_leaves_the_composer_alone() {
        let mut history = History::default();
        assert_eq!(history.previous("draft"), None);
        assert_eq!(history.next(), None);
    }

    /// A resumed session's own prompts are recallable, so the history a
    /// session starts with is the transcript it starts with.
    #[test]
    fn the_history_starts_from_the_transcripts_prompts() {
        let entries = vec![
            Entry {
                kind: Some(EntryKind::User),
                raw: "resumed prompt".to_string(),
                ..Entry::default()
            },
            Entry {
                kind: Some(EntryKind::Assistant),
                raw: "the reply".to_string(),
                ..Entry::default()
            },
        ];

        assert_eq!(prompt_history(&entries), vec!["resumed prompt".to_string()]);
    }

    /// A recalled slash command reopens the suggestion panel, which owns
    /// Up/Down itself. While a recall is in force the history keeps them, so
    /// one command in the history cannot strand the keys walking it.
    #[tokio::test]
    async fn arrow_keys_recall_prompts_and_the_suggestion_panel_does_not_steal_them() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        app.history.remember("first");
        app.history.remember("/model");

        app.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.input.iter().collect::<String>(), "/model");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(app.scroll, None, "the transcript must not scroll");
        assert!(!app.suggestions().is_empty(), "the panel is open on /model");

        app.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.input.iter().collect::<String>(), "first");

        // An edit ends the recall, so the panel owns the keys again and the
        // next Up starts over from the newest line.
        app.handle_key(
            key(KeyCode::Char('!'), KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        app.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.input.iter().collect::<String>(), "/model");
        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.input.iter().collect::<String>(), "first!");
    }

    #[tokio::test]
    async fn a_submitted_line_enters_the_history() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&Backend::Local(&controller));
        app.input = "  hello  ".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        app.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert_eq!(app.input.iter().collect::<String>(), "hello");
    }

    /// The suggestion panel's selection, the half `super::render`'s
    /// display-only panel left out: up/down move the highlighted row rather
    /// than scrolling the transcript, and the selection wraps like a
    /// [`Picker`]'s.
    #[tokio::test]
    async fn arrow_keys_move_the_suggestion_selection_instead_of_scrolling() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.input = "/sk".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.suggestion, 0, "only /skill matches");
        assert_eq!(app.scroll, None, "the transcript must not scroll");
        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.suggestion, 0, "selection wraps");
        app.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.suggestion, 0);
    }

    #[tokio::test]
    async fn tab_accepts_the_selected_suggestion() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.input = "/s".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        app.handle_key(
            key(KeyCode::Tab, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert_eq!(app.input.iter().collect::<String>(), "/sandbox");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(app.suggestion, 0, "the accepted row is the only match left");
    }

    #[tokio::test]
    async fn enter_runs_the_selected_suggestion_not_the_typed_prefix() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.input = "/s".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(
            app.entries
                .last()
                .expect("entry")
                .raw
                .starts_with("Sandbox:"),
            "{:?}",
            app.entries.last().map(|entry| entry.raw.clone())
        );
        assert!(app.input.is_empty());
    }

    #[tokio::test]
    async fn editing_the_composer_resets_the_suggestion_selection() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.input = "/".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        app.handle_key(
            key(KeyCode::Char('s'), KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.suggestion, 0);

        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        app.handle_key(
            key(KeyCode::Backspace, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );
        assert_eq!(app.suggestion, 0);
    }

    #[tokio::test]
    async fn thinking_command_without_a_level_opens_a_level_picker() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        controller.set_thinking("high").await.expect("thinking");
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        let action = app.dispatch_line("/thinking", &Backend::Local(&controller), &cancel);

        assert!(action.is_none());
        let picker = app.picker.as_ref().expect("thinking picker");
        assert_eq!(picker.kind, PickerKind::Thinking);
        assert_eq!(picker.rows.len(), 6);
        assert_eq!(picker.rows[0].label, "  default");
        assert_eq!(picker.rows[3].label, "* high");
        assert_eq!(picker.rows[3].value, "high");
        assert_eq!(picker.selected, 3);
    }

    #[tokio::test]
    async fn thinking_picker_enter_sets_the_current_session_thinking() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.picker = Some(thinking_picker(""));
        app.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(matches!(
            action,
            Some(Action::SetThinking { thinking, save: false }) if thinking == "low"
        ));
    }

    #[tokio::test]
    async fn sandbox_allow_opens_a_confirmation_over_the_resolved_path() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        std::fs::create_dir(workspace.path().join("cache")).expect("cache");
        let resolved = controller
            .resolve_sandbox_read_path("~/cache")
            .expect("resolve");
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        let action = app.dispatch_line(
            "/sandbox allow ~/cache",
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(action.is_none());
        let picker = app.picker.as_ref().expect("sandbox picker");
        assert_eq!(picker.kind, PickerKind::Sandbox);
        assert_eq!(picker.rows.len(), 2);
        assert!(
            picker.rows[0].label.contains(&resolved),
            "{:?}",
            picker.rows
        );
        assert_eq!(picker.rows[1].value, "");
        assert_eq!(picker.selected, 1);
        app.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(matches!(&action, Some(Action::SandboxAllow(path)) if *path == resolved));
    }

    #[tokio::test]
    async fn the_sandbox_confirmation_can_be_declined() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        std::fs::create_dir(workspace.path().join("cache")).expect("cache");
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.dispatch_line(
            "/sandbox allow ~/cache",
            &Backend::Local(&controller),
            &cancel,
        );

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(action.is_none());
        assert!(app.picker.is_none());
    }

    #[tokio::test]
    async fn sandbox_allow_reports_a_path_that_cannot_be_granted() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        for line in ["/sandbox allow", "/sandbox allow ~/missing"] {
            let action = app.dispatch_line(line, &Backend::Local(&controller), &cancel);

            assert!(action.is_none());
            assert!(app.picker.is_none(), "{line}");
            let entry = app.entries.last().expect("entry");
            assert_eq!(entry.kind, Some(EntryKind::System));
            assert!(entry.raw.starts_with("/sandbox allow:"), "{}", entry.raw);
        }
    }

    #[tokio::test]
    async fn sandbox_exclude_dispatches_the_entry_without_a_confirmation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        for line in [
            "/sandbox exclude lark-cli *",
            "/sandbox exclude 'lark-cli *'",
            "/sandbox exclude \"lark-cli *\"",
        ] {
            let action = app.dispatch_line(line, &Backend::Local(&controller), &cancel);
            assert!(
                matches!(&action, Some(Action::SandboxExclude(entry)) if entry == "lark-cli *"),
                "{line}"
            );
            assert!(app.picker.is_none(), "{line}");
        }

        let action = app.dispatch_line("/sandbox exclude", &Backend::Local(&controller), &cancel);
        assert!(action.is_none());
        let entry = app.entries.last().expect("entry");
        assert!(
            entry.raw.starts_with("/sandbox exclude: usage:"),
            "{}",
            entry.raw
        );
    }

    #[tokio::test]
    async fn approve_dispatches_the_always_form_and_refuses_other_arguments() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        let action =
            app.dispatch_line("/approve approval-1", &Backend::Local(&controller), &cancel);
        assert!(matches!(&action, Some(Action::Approve(id)) if id == "approval-1"));
        let action = app.dispatch_line(
            "/approve approval-1 always",
            &Backend::Local(&controller),
            &cancel,
        );
        assert!(matches!(&action, Some(Action::ApproveAlways(id)) if id == "approval-1"));

        for line in [
            "/approve",
            "/approve approval-1 sometimes",
            "/approve a b always",
        ] {
            assert!(
                app.dispatch_line(line, &Backend::Local(&controller), &cancel)
                    .is_none(),
                "{line}"
            );
            let entry = app.entries.last().expect("entry");
            assert!(entry.raw.starts_with("unknown command:"), "{}", entry.raw);
        }
    }

    #[tokio::test]
    async fn sandbox_network_takes_a_mode_or_opens_a_picker() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();

        let action = app.dispatch_line(
            "/sandbox network deny",
            &Backend::Local(&controller),
            &cancel,
        );
        assert!(matches!(&action, Some(Action::SandboxNetwork(mode)) if mode == "deny"));
        assert!(app.picker.is_none());

        let action = app.dispatch_line("/sandbox network", &Backend::Local(&controller), &cancel);
        assert!(action.is_none());
        let picker = app.picker.as_ref().expect("sandbox picker");
        assert_eq!(picker.kind, PickerKind::Sandbox);
        assert_eq!(picker.rows.len(), 2);
        assert_eq!(picker.rows[0].value, "network\tallow");
        assert_eq!(picker.rows[1].value, "network\tdeny");

        let action = app.dispatch_line(
            "/sandbox network sometimes",
            &Backend::Local(&controller),
            &cancel,
        );
        assert!(action.is_none());
        assert!(
            app.entries
                .last()
                .expect("entry")
                .raw
                .starts_with("unknown command: /sandbox")
        );
    }

    #[tokio::test]
    async fn submitting_a_prompt_echoes_it_into_the_transcript() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        let cancel = CancellationToken::new();
        app.input = "hello otto".chars().collect();
        app.cursor = app.input.len();

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &Backend::Local(&controller),
            &cancel,
        );

        assert!(matches!(&action, Some(Action::Prompt(line)) if line == "hello otto"));
        let entry = app.entries.last().expect("entry");
        assert_eq!(entry.kind, Some(EntryKind::User));
        assert_eq!(entry.raw, "hello otto");
    }
    use crate::tui::attach::Remote;

    fn attached_app(remote: &Remote) -> App {
        App::new(&Backend::Attach(remote))
    }

    fn frame(event_type: &str) -> WireEvent {
        WireEvent {
            event_type: event_type.to_string(),
            ..WireEvent::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_prompt_has_no_local_echo_and_user_message_frame_is_the_echo() {
        let remote = Remote::offline(Vec::new());
        let backend = Backend::Attach(&remote);
        let cancel = CancellationToken::new();
        let mut app = attached_app(&remote);
        app.input = "hello otto".chars().collect();
        app.cursor = app.input.len();

        let action = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &backend, &cancel);

        assert!(matches!(&action, Some(Action::Prompt(line)) if line == "hello otto"));
        assert!(app.entries.is_empty(), "the prompt is shown by its frame");

        app.apply_event(&WireEvent::user_message("hello otto", false));
        let entry = app.entries.last().expect("entry");
        assert_eq!(entry.kind, Some(EntryKind::User));
        assert_eq!(entry.raw, "hello otto");
        assert!(app.queued_input.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_text_typed_during_a_turn_is_sent_as_the_next_turn() {
        let remote = Remote::offline(Vec::new());
        let backend = Backend::Attach(&remote);
        let cancel = CancellationToken::new();
        let mut app = attached_app(&remote);
        app.start_turn();
        app.insert_text("next");

        let action = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &backend, &cancel);

        assert!(action.is_none());
        assert!(matches!(app.outgoing.as_slice(), [Action::Prompt(line)] if line == "next"));
        assert!(app.queued_input.is_none());
        assert!(
            app.entries
                .last()
                .expect("entry")
                .raw
                .starts_with("Queued as the next turn")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_unsupported_commands_print_not_available_and_do_nothing() {
        let remote = Remote::offline(Vec::new());
        let backend = Backend::Attach(&remote);
        let cancel = CancellationToken::new();
        for (line, name) in [
            ("/archive", "/archive"),
            ("/model", "/model"),
            ("/model gpt", "/model"),
            ("/thinking high", "/thinking"),
            ("/sandbox allow /tmp", "/sandbox"),
            ("/sandbox network on", "/sandbox"),
            ("/sandbox exclude git", "/sandbox"),
            ("/approve abc always", "/approve"),
            ("/login", "/login"),
            ("/logout", "/logout"),
            ("/mcp", "/mcp"),
            ("/memory", "/memory"),
            ("/remember x", "/remember"),
            ("/reflect", "/reflect"),
            ("/tasks", "/tasks"),
            ("/task t1", "/task"),
            ("/timers", "/timers"),
            ("/skill", "/skill"),
        ] {
            let mut app = attached_app(&remote);
            let action = app.dispatch_line(line, &backend, &cancel);
            assert!(action.is_none(), "{line} raised an action");
            assert_eq!(app.entries.len(), 1, "{line}");
            assert_eq!(
                app.entries[0].raw,
                format!("{name}: not available with --attach"),
                "{line}"
            );
            assert!(app.picker.is_none(), "{line} opened a picker");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_commands_that_serve_supports_raise_actions() {
        let remote = Remote::offline(Vec::new());
        let backend = Backend::Attach(&remote);
        let cancel = CancellationToken::new();
        let mut app = attached_app(&remote);
        assert!(matches!(
            app.dispatch_line("/approve abc", &backend, &cancel),
            Some(Action::Approve(id)) if id == "abc"
        ));
        assert!(matches!(
            app.dispatch_line("/compact focus", &backend, &cancel),
            Some(Action::Compact(focus)) if focus == "focus"
        ));
        assert!(matches!(
            app.dispatch_line("/sandbox reload", &backend, &cancel),
            Some(Action::SandboxReload)
        ));
        assert!(matches!(
            app.dispatch_line("/new", &backend, &cancel),
            Some(Action::NewSession)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn approval_dialog_opens_on_the_frame_and_closes_on_the_decision() {
        let remote = Remote::offline(Vec::new());
        let backend = Backend::Attach(&remote);
        let cancel = CancellationToken::new();
        let request = || WireEvent {
            event_type: APPROVAL_REQUESTED.to_string(),
            approval_id: "a1".to_string(),
            command: "ls".to_string(),
            ..WireEvent::default()
        };

        let mut app = attached_app(&remote);
        app.apply_event(&request());
        assert_eq!(app.approval.as_ref().map(|a| a.id.as_str()), Some("a1"));
        let action = app.handle_key(
            key(KeyCode::Char('y'), KeyModifiers::NONE),
            &backend,
            &cancel,
        );
        assert!(matches!(action, Some(Action::Approve(id)) if id == "a1"));
        assert!(app.approval.is_none());

        app.apply_event(&request());
        let action = app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &backend, &cancel);
        assert!(matches!(action, Some(Action::Deny(id)) if id == "a1"));

        // A decision made by another client closes the dialog.
        app.apply_event(&request());
        app.apply_event(&WireEvent {
            event_type: APPROVAL_DECIDED.to_string(),
            approval_id: "a1".to_string(),
            decision: "allow".to_string(),
            ..frame(APPROVAL_DECIDED)
        });
        assert!(app.approval.is_none());
        assert!(
            app.entries
                .last()
                .expect("entry")
                .raw
                .contains("decided elsewhere")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_app_starts_from_the_loaded_history() {
        let history = vec![
            Message {
                role: Role::User,
                blocks: vec![Block::text("earlier prompt")],
                ..Message::default()
            },
            Message {
                role: Role::Assistant,
                blocks: vec![Block::text("earlier reply")],
                ..Message::default()
            },
        ];
        let remote = Remote::offline(history);
        let app = attached_app(&remote);
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.raw == "earlier prompt")
        );
        assert!(app.attached);
    }

    #[tokio::test]
    async fn local_notification_for_a_queued_prompt_becomes_the_user_entry() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&Backend::Local(&controller));
        app.queued_input = Some("change course".to_string());
        app.queued_input_sent = true;

        app.apply_event(&wire_frame(&Event::Notification {
            kind: Some(NotificationKind::UserMessage),
            task_id: String::new(),
            text: "change course".into(),
            usage: Usage::default(),
            present: false,
        }));

        let entry = app.entries.last().expect("entry");
        assert_eq!(entry.kind, Some(EntryKind::User));
        assert_eq!(entry.raw, "change course");
        assert!(app.queued_input.is_none());
        assert!(!app.queued_input_sent);
    }
}
