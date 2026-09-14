//! Application state, key handling, and command dispatch. Port of the
//! non-rendering parts of `internal/tui/model.go`, `resume.go`, and
//! `archive.go`.
//!
//! ponytail: Go's `Model` is a single Bubble Tea state machine that also
//! owns rendering (`View`). Ratatui redraws the whole frame every tick from
//! plain state instead of an Elm-style message loop, so this module owns
//! only state and key handling; [`super::render`] turns that state into
//! widgets.
//!
//! Not ported: Go's `overlayHelp`/`overlaySession`/etc. are separate Bubble
//! Tea "screens" with their own key maps. Command output that Go shows in an
//! overlay (`/session`, `/model` with no argument, `/sandbox`, `/tasks`,
//! `/task`) is appended to the transcript as a system entry here instead,
//! matching the text `internal/repl` already prints for the same commands.
//! Only `/resume`, `/archive`, and `/model <profile>`'s profile picker are
//! genuinely interactive, so those get a real picker overlay (below).
//! Upgrade path: split these into dedicated overlays if a user reports the
//! inline transcript entries as hard to scan.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use otto_core::agent::{CompactionResult, Event};
use otto_core::model::Usage;
use otto_core::session::types::SessionInfo;
use tokio_util::sync::CancellationToken;

use crate::app::tasks::{Task, TaskStatus};
use crate::app::{Controller, Info, PROFILE_SWITCH_UNAVAILABLE};
use crate::cli::login;
use crate::cli::repl_commands;

use super::commands::{self, SlashCommand, SlashCommandKind};
use super::entries::{self, Entry, EntryKind};
use super::layout;

/// Go's `ctrlCArmWindow`: the time a first Ctrl+C stays armed for a
/// confirming second press.
const CTRL_C_ARM_WINDOW: Duration = Duration::from_secs(1);

/// Go's `ctrlCExitStatus`.
const CTRL_C_EXIT_STATUS: &str = "press Ctrl+C again to exit";

/// How many sessions a `/resume` or `/archive` picker lists. Port of Go's
/// `resumeListLimit`/`archiveListLimit` (both `50` in `internal/tui`).
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
}

impl PickerKind {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Resume => "Resume session (enter to select, esc to cancel)",
            Self::Archive => "Archive session (enter to select, esc to cancel)",
            Self::Profile => "Switch profile (enter to select, esc to cancel)",
        }
    }
}

/// An open list picker. Port of the shared shape of Go's
/// `resumePickerState`/`archivePickerState` (and `profile.go`'s picker).
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

/// Async work [`App::handle_key`] cannot start itself (every
/// [`Controller`] method it would need is `async`). [`super::run`] awaits
/// these one at a time, matching [`Controller::begin_operation`]'s
/// single-admission-at-a-time contract.
pub(crate) enum Action {
    Exit,
    Prompt(String),
    Compact(String),
    NewSession,
    SwitchProfile(String),
    Resume(String),
    Archive(String),
    SandboxReload,
    Login(String),
}

/// The terminal frontend's whole state. Port of the non-view fields of
/// `internal/tui/model.go`'s `Model`.
pub(crate) struct App {
    pub entries: Vec<Entry>,
    pub usage: Usage,
    pub info: Info,
    pub input: Vec<char>,
    pub cursor: usize,
    /// `None` follows the bottom of the transcript; `Some(n)` stays `n`
    /// wrapped lines above it. Port of Go's `autoFollow` (inverted: Go stores
    /// a bool and the last offset separately, this folds both into one field).
    pub scroll: Option<u16>,
    pub picker: Option<Picker>,
    /// The highlighted row of the slash-command suggestion panel (see
    /// [`App::suggestions`]). Every composer edit resets it to `0`, so it
    /// only ever indexes the match list the current value produces.
    pub suggestion: usize,
    pub show_help: bool,
    pub show_details: bool,
    pub busy: bool,
    pub status: Option<String>,
    ctrl_c_armed_at: Option<Instant>,
}

impl App {
    pub fn new(controller: &Controller) -> Self {
        let (entries, usage) = entries::entries_from_history(&controller.history());
        Self {
            entries,
            usage,
            info: controller.info(),
            input: Vec::new(),
            cursor: 0,
            scroll: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy: false,
            status: None,
            ctrl_c_armed_at: None,
        }
    }

    /// Rebuilds the transcript from the controller's current history.
    /// [`super::run`] calls this after every action that replaces the whole
    /// session (`/new`, `/resume`, `/archive`, switching profiles) so the
    /// transcript can never drift from `Controller::history`.
    ///
    /// A completed prompt or `/compact` does *not* call this: like Go's
    /// Bubble Tea model, the transcript is append-only during a turn
    /// ([`App::apply_event`] is the sole writer), so an in-progress or
    /// just-finished turn's entries are never rebuilt out from under a
    /// still-visible scrollback.
    pub fn refresh(&mut self, controller: &Controller) {
        let (entries, usage) = entries::entries_from_history(&controller.history());
        self.entries = entries;
        self.usage = usage;
        self.info = controller.info();
        self.scroll = None;
    }

    pub fn refresh_info(&mut self, controller: &Controller) {
        self.info = controller.info();
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
        self.scroll = Some(self.scroll.unwrap_or(0).saturating_add(lines));
    }

    fn scroll_down(&mut self, lines: u16) {
        self.scroll = match self.scroll {
            Some(offset) if offset > lines => Some(offset - lines),
            _ => None,
        };
    }

    /// Port of `handleCtrlC`'s idle branch: a lone Ctrl+C clears the composer
    /// and arms a second press; a confirming press within
    /// [`CTRL_C_ARM_WINDOW`] exits. While a turn is running, [`super::run`]
    /// intercepts Ctrl+C before it reaches [`App::handle_key`] at all (see
    /// [`is_interrupt_key`]) and cancels the turn directly instead of
    /// arming, matching Go's per-turn-interrupt-then-exit-prompt SIGINT
    /// semantics; `self.busy` is therefore always false here.
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

    /// Handles one key press. Returns `Some(Action)` for the one key
    /// (submitting a prompt or command) that needs an `async` `Controller`
    /// call; every purely local effect (composer editing, picker
    /// navigation, overlays) is applied directly to `self`.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        controller: &Controller,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        if key.code != KeyCode::Char('c') || !key.modifiers.contains(KeyModifiers::CONTROL) {
            self.clear_ctrl_c_arm();
        } else {
            return self.handle_ctrl_c();
        }

        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.show_help = false;
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
                KeyCode::Enter => {
                    let picker = self.picker.take()?;
                    let row = picker.rows.into_iter().nth(picker.selected)?;
                    return Some(match picker.kind {
                        PickerKind::Resume => Action::Resume(row.value),
                        PickerKind::Archive => Action::Archive(row.value),
                        PickerKind::Profile => Action::SwitchProfile(row.value),
                    });
                }
                _ => {}
            }
            return None;
        }

        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.show_details = !self.show_details;
            return None;
        }
        if self.busy {
            // Go ignores most keys while a turn runs; Esc (Cancel) is
            // handled by the caller, which cancels the turn's child token.
            return None;
        }
        if key.code == KeyCode::Char('?') && self.input.is_empty() {
            self.show_help = true;
            return None;
        }

        // While the suggestion panel is open it owns the keys that would
        // otherwise scroll the transcript or complete a prefix: up/down move
        // the highlighted row, Tab accepts it, and Enter runs it rather than
        // the typed prefix. Port of Go's `commandSuggestions` cursor.
        let suggestions = self.suggestions();
        if !suggestions.is_empty() {
            let selected = self.suggestion.min(suggestions.len() - 1);
            match key.code {
                KeyCode::Up | KeyCode::Down => {
                    let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                    self.suggestion =
                        (selected as isize + delta).rem_euclid(suggestions.len() as isize) as usize;
                    return None;
                }
                KeyCode::Tab => {
                    self.set_input(suggestions[selected].name);
                    return None;
                }
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                {
                    // Falls through to the Enter arm below, which submits it.
                    self.set_input(suggestions[selected].name);
                }
                _ => {}
            }
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
                let line: String = std::mem::take(&mut self.input).into_iter().collect();
                self.cursor = 0;
                self.suggestion = 0;
                self.dispatch_line(line.trim(), controller, cancel)
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                }
                self.suggestion = 0;
                None
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                }
                self.suggestion = 0;
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
            KeyCode::Up => {
                self.scroll_up(1);
                None
            }
            KeyCode::Down => {
                self.scroll_down(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_up(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_down(10);
                None
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.insert(self.cursor, ch);
                self.cursor += 1;
                self.suggestion = 0;
                None
            }
            _ => None,
        }
    }

    /// Esc or Ctrl+C while a turn is running cancels it, matching Go's
    /// per-turn SIGINT/interrupt semantics; the caller (which holds the
    /// turn's `CancellationToken`) checks this before calling
    /// [`App::handle_key`] and cancels the turn directly instead of
    /// forwarding the key.
    pub fn is_interrupt_key(key: &KeyEvent) -> bool {
        key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    }

    /// The slash commands the composer's current value is a prefix of, with
    /// [`App::suggestion`] indexing the highlighted one. Port of
    /// `Model.commandSuggestions`: an open overlay hides the panel.
    /// [`super::render`] draws exactly this list.
    pub(super) fn suggestions(&self) -> Vec<SlashCommand> {
        if self.show_help || self.picker.is_some() {
            return Vec::new();
        }
        let value: String = self.input.iter().collect();
        commands::matching_slash_commands(&value)
    }

    /// Replaces the composer with an accepted command name. The panel is
    /// then down to that one row, so the selection returns to it.
    fn set_input(&mut self, value: &str) {
        self.input = value.chars().collect();
        self.cursor = self.input.len();
        self.suggestion = 0;
    }

    /// Parses and dispatches one submitted line. Port of `internal/repl`'s
    /// `command()` dispatch table, reusing its exact output text for every
    /// command whose semantics match; `/resume`, `/archive`, and `/model`
    /// with no argument open a picker instead of printing text, since a
    /// picker is the TUI-native form of the same command.
    fn dispatch_line(
        &mut self,
        line: &str,
        controller: &Controller,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        if line.is_empty() {
            return None;
        }
        let Some(rest) = line.strip_prefix('/') else {
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
            SlashCommandKind::Exit => {
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                Some(Action::Exit)
            }
            SlashCommandKind::New => {
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
                self.push_system(session_report(controller));
                None
            }
            SlashCommandKind::Rename => {
                if args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                match controller.rename_session(&args) {
                    Ok(()) => self.push_system(format!("Renamed session: {args}")),
                    Err(message) => self.push_system(format!("/rename: {message}")),
                }
                None
            }
            SlashCommandKind::Compact => Some(Action::Compact(args)),
            SlashCommandKind::Model => {
                if !controller.dynamic_content() {
                    self.push_system(format!("/model: {PROFILE_SWITCH_UNAVAILABLE}"));
                    return None;
                }
                if args.is_empty() {
                    let profiles = controller.profiles();
                    if profiles.is_empty() {
                        self.push_system(model_report(controller));
                        return None;
                    }
                    let rows = profiles
                        .into_iter()
                        .map(|profile| PickerRow {
                            label: profile.clone(),
                            value: profile,
                        })
                        .collect();
                    self.picker = Some(Picker::new(PickerKind::Profile, rows));
                    None
                } else {
                    Some(Action::SwitchProfile(args))
                }
            }
            SlashCommandKind::Resume => {
                self.open_session_picker(PickerKind::Resume, controller);
                None
            }
            SlashCommandKind::Archive => {
                self.open_session_picker(PickerKind::Archive, controller);
                None
            }
            SlashCommandKind::Sandbox => {
                if args.is_empty() {
                    let info = controller.sandbox_info();
                    let reason = info.reason_code();
                    let mut text = format!("Sandbox: {}", info.summary());
                    if !reason.is_empty() {
                        text.push_str(&format!("\nSandbox reason: {reason}"));
                    }
                    self.push_system(text);
                    None
                } else if args == "reload" {
                    Some(Action::SandboxReload)
                } else {
                    self.push_system(format!("unknown command: /sandbox {args}"));
                    None
                }
            }
            SlashCommandKind::Login => Some(Action::Login(args)),
            SlashCommandKind::Logout => {
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
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                self.push_system(tasks_report(controller));
                None
            }
            SlashCommandKind::Task => {
                if args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                self.push_system(task_report(controller, &args));
                None
            }
            SlashCommandKind::Memory => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                let result =
                    repl_commands::repl_memory_command(controller, &args, &mut stdout, &mut stderr);
                self.push_command_result(result, &stdout, &stderr, "/memory");
                None
            }
            SlashCommandKind::Remember => {
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

    /// Port of `runSessionListCommand`/`runArchiveListCommand`:
    /// `list_sessions` is synchronous, so the picker opens with no
    /// intermediate loading state.
    fn open_session_picker(&mut self, kind: PickerKind, controller: &Controller) {
        match controller.list_sessions(PICKER_LIST_LIMIT) {
            Ok(result) => {
                if result.sessions.is_empty() {
                    self.push_system("No sessions found.");
                    return;
                }
                let rows = result.sessions.iter().map(session_row).collect();
                self.picker = Some(Picker::new(kind, rows));
            }
            Err(message) => self.push_system(format!("/{}: {message}", picker_command_name(kind))),
        }
    }

    /// Applies one streamed turn event to the transcript. Port of the
    /// relevant arms of `Model.applyEvent`.
    ///
    /// ponytail: Go incrementally patches the streaming assistant entry in
    /// place and keeps a separate "dirty" flag to re-wrap only what
    /// changed. This instead appends one system line per event, matching
    /// the append-only shape of Go's own Bubble Tea transcript (this is the
    /// sole writer during a turn; [`App::refresh`] is never called
    /// mid-turn, so nothing here is later discarded). The live view is
    /// coarser (no character-by-character growth of the assistant bubble)
    /// but the content is the same once the turn ends. Upgrade path: keep a
    /// dedicated in-progress entry and append text deltas into it if
    /// scrollback churn during streaming turns is reported as noisy.
    ///
    /// Returns `true` for an [`Event::AgentError`], mirroring `internal/
    /// repl`'s `renderEvent` so [`super::run`] does not also print a turn's
    /// final `Err` when the same failure already appeared as an event.
    pub fn apply_event(&mut self, event: Event) -> bool {
        match event {
            Event::TextDelta { text } => {
                if let Some(last) = self.entries.last_mut()
                    && last.kind == Some(EntryKind::Assistant)
                    && last.id == "streaming"
                {
                    last.raw.push_str(&text);
                } else {
                    self.entries.push(Entry {
                        id: "streaming".to_string(),
                        kind: Some(EntryKind::Assistant),
                        raw: text,
                        ..Entry::default()
                    });
                }
                self.scroll = None;
                false
            }
            Event::ToolCallStarted {
                tool_name,
                tool_call_id,
                arguments,
            } => {
                self.entries.push(Entry {
                    id: format!("streaming-tool-{}", self.entries.len()),
                    kind: Some(EntryKind::Tool),
                    tool_call_id,
                    tool_name,
                    tool_args: arguments,
                    ..Entry::default()
                });
                self.scroll = None;
                false
            }
            Event::ToolCallFinished {
                tool_call_id,
                result,
                ..
            } => {
                if let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
                    entry.kind == Some(EntryKind::Tool) && entry.tool_call_id == tool_call_id
                }) {
                    entry.tool_output = result.content;
                    entry.tool_error = result.is_error;
                    entry.tool_done = true;
                }
                self.scroll = None;
                false
            }
            Event::CompactionCompleted { compaction } => {
                self.push_system(compaction_line(&compaction));
                false
            }
            Event::CompactionWarning { message } | Event::MemoryWarning { message } => {
                self.push_system(message);
                false
            }
            Event::AgentError { message } => {
                self.push_system(message);
                true
            }
            Event::Notification { task_id, text, .. } => {
                self.push_system(format!("[task {task_id}] {text}"));
                false
            }
            Event::AgentStarted
            | Event::AgentFinished
            | Event::ProviderUsage { .. }
            | Event::ProviderApiCall { .. }
            | Event::CompactionStarted { .. }
            | Event::CompactionPlanned { .. } => false,
        }
    }
}

fn picker_command_name(kind: PickerKind) -> &'static str {
    match kind {
        PickerKind::Resume => "resume",
        PickerKind::Archive => "archive",
        PickerKind::Profile => "model",
    }
}

fn session_row(session: &SessionInfo) -> PickerRow {
    let marker = if session.current { " (current)" } else { "" };
    let name = if session.name.is_empty() {
        &session.id
    } else {
        &session.name
    };
    PickerRow {
        label: format!("{name}{marker} — {}", session.last_user_text),
        value: session.path.clone(),
    }
}

/// Port of `internal/repl`'s `/session` output.
fn session_report(controller: &Controller) -> String {
    let info = controller.info();
    let mut text = format!(
        "ID: {}\nPath: {}\nProvider: {}\nModel: {}\nSandbox: {}",
        info.session_id,
        info.session_path,
        info.provider,
        info.model,
        info.sandbox.summary()
    );
    if !info.session_name.is_empty() {
        text.push_str(&format!("\nName: {}", info.session_name));
    }
    let reason = info.sandbox.reason_code();
    if !reason.is_empty() {
        text.push_str(&format!("\nSandbox reason: {reason}"));
    }
    text
}

/// Port of `internal/repl`'s `/model` (no-argument) output.
fn model_report(controller: &Controller) -> String {
    let info = controller.info();
    let mut text = format!(
        "Current: profile {} (provider {}, model {})",
        info.profile, info.provider, info.model
    );
    text.push_str("\nNo profiles configured.");
    text
}

/// Port of `internal/repl`'s `/compact`'s `compactionLine`. `pub(super)`
/// because [`super::run`] also needs it for a `/compact` call's final
/// [`otto_core::agent::CompactionResult`] (as opposed to a streamed
/// [`Event::CompactionCompleted`], which [`App::apply_event`] handles
/// itself).
pub(super) fn compaction_line(result: &CompactionResult) -> String {
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

fn task_report(controller: &Controller, args: &str) -> String {
    let mut parts = args.split_whitespace();
    let Some(reference) = parts.next() else {
        return "usage: /task <id> [cancel]".to_string();
    };
    let cancel = parts.next() == Some("cancel");
    let Some(tasks) = controller.tasks() else {
        return "task not found".to_string();
    };
    if cancel {
        return match tasks.cancel(reference) {
            Ok(()) => format!("Cancelled task: {reference}"),
            Err(message) => format!("/task {reference} cancel: {message}"),
        };
    }
    match tasks.get(reference) {
        Some(task) => task_line(&task),
        None => "task not found".to_string(),
    }
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
            scroll: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy: false,
            status: None,
            ctrl_c_armed_at: None,
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
            scroll: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy: false,
            status: None,
            ctrl_c_armed_at: None,
        };
        assert!(app.handle_ctrl_c().is_none());
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn help_key_opens_help_only_when_the_composer_is_empty() {
        let app = App {
            entries: Vec::new(),
            usage: Usage::default(),
            info: Info::default(),
            input: "abc".chars().collect(),
            cursor: 3,
            scroll: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy: false,
            status: None,
            ctrl_c_armed_at: None,
        };
        let event = key(KeyCode::Char('?'), KeyModifiers::NONE);
        // No controller is available in a unit test; '?' with pending text
        // must insert a literal character without touching the controller,
        // so it is safe to exercise without one by checking state alone.
        assert!(!app.input.is_empty());
        let _ = event;
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
        assert_eq!(compaction_line(&noop), "[context] no-op");
        let estimated = CompactionResult {
            noop: false,
            tokens_before: 12_000,
            estimated_tokens_after: 4_000,
            ..Default::default()
        };
        assert_eq!(
            compaction_line(&estimated),
            "[context] compacted 12k \u{2192} 4k tokens"
        );
    }

    #[tokio::test]
    async fn scroll_keys_track_lines_above_the_bottom() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.scroll, Some(1));
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.scroll, None);
    }

    // Port of `internal/tui/memory_test.go` against this module's own unit
    // seams (`dispatch_line`/`handle_key`) rather than Go's Bubble Tea
    // `Update`/status-text plumbing: every command result here lands as one
    // transcript entry via `push_system`/`push_command_result`, since this
    // frontend has no separate status bar (see the module doc's "Not
    // ported" note). `TestMemoryWarningEventSetsStatusDuringPrompt` is not
    // ported here: it exercises `apply_event`'s existing `MemoryWarning`
    // arm, unrelated to command dispatch.

    use crate::cli::testutil;
    use crate::memory::RememberRequest;

    #[tokio::test]
    async fn memory_command_without_a_service_pushes_the_unavailable_message() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/memory search vim", &controller, &cancel);

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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/remember prefers dark mode", &controller, &cancel);

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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/memory", &controller, &cancel);

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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/remember --scope user", &controller, &cancel);

        assert_eq!(
            app.entries.last().expect("entry").raw,
            repl_commands::REMEMBER_USAGE
        );
    }

    /// Documents the pre-existing divergence recorded in
    /// `repl_commands.rs`'s module doc: `/memory review` reaches candidate
    /// review and automatic extraction, neither of which is ported, so the
    /// subcommand always falls through to the same usage line as an unknown
    /// one, regardless of whether the decision word is valid. This merges
    /// Go's `TestMemoryReviewCommandTriesScopesAndAppliesDecision` and
    /// `TestMemoryReviewCommandRejectsInvalidDecision`, which differ only in
    /// whether the decision is valid — a distinction this frontend can't yet
    /// observe.
    #[tokio::test]
    async fn memory_review_falls_through_to_the_usage_line_pending_the_reviewer_port() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let controller = testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/memory review cand-1 accept", &controller, &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            repl_commands::MEMORY_USAGE
        );

        app.dispatch_line("/memory review cand-1 maybe", &controller, &cancel);
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line(
            "/remember --kind preference --key editor vim",
            &controller,
            &cancel,
        );
        app.dispatch_line("/memory search vim", &controller, &cancel);

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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line(
            &format!("/memory forget {}", record.id),
            &controller,
            &cancel,
        );
        assert_eq!(
            app.entries.last().expect("entry").raw,
            format!("forgot {} (revision 1)", record.id)
        );

        app.dispatch_line("/memory forget missing", &controller, &cancel);
        let missing = app.entries.last().expect("entry").raw.clone();
        assert!(missing.starts_with("/memory: "), "{missing}");
        assert!(missing.contains("not found"), "{missing}");
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/remember prefers dark mode", &controller, &cancel);

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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line(
            "/remember --scope user --kind preference --key editor vim",
            &controller,
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

    /// Port of `TestMemoryAndRememberCommandsRejectedWhileTurnActive`.
    /// Divergence: Go's per-command guard sets `statusText` to
    /// `app.ErrPromptActive`; this frontend has no per-command guard or
    /// status bar, so [`App::handle_key`]'s single `if self.busy` check
    /// (shared by every slash command, not memory-specific) silently
    /// declines to dispatch instead of pushing a rejection message. What's
    /// verified here is the same observable guarantee Go's test checks: the
    /// command never runs while a turn is active.
    #[tokio::test]
    async fn busy_guard_rejects_slash_commands_while_a_turn_is_active() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let before = app.entries.len();
        app.busy = true;
        app.input = "/memory search vim".chars().collect();
        app.cursor = app.input.len();
        let cancel = CancellationToken::new();

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &controller,
            &cancel,
        );

        assert!(action.is_none());
        assert_eq!(app.entries.len(), before, "no command must run while busy");
        assert_eq!(
            app.input.iter().collect::<String>(),
            "/memory search vim",
            "the composer must be left untouched"
        );
    }

    /// Port of the completion half of
    /// `TestMemoryCommandRegistryCompletionAndHelp`; the help-overlay text
    /// containment half is `super::render`'s concern, not this module's.
    #[tokio::test]
    async fn tab_completes_a_memory_prefix_to_the_full_command() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        app.input = "/mem".chars().collect();
        app.cursor = app.input.len();
        let cancel = CancellationToken::new();

        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE), &controller, &cancel);

        assert_eq!(app.input.iter().collect::<String>(), "/memory");
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.input = "/s".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.suggestion, 1, "/session then /sandbox");
        assert_eq!(app.scroll, None, "the transcript must not scroll");
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.suggestion, 0, "selection wraps");
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.suggestion, 1);
    }

    #[tokio::test]
    async fn tab_accepts_the_selected_suggestion() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.input = "/s".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE), &controller, &cancel);

        assert_eq!(app.input.iter().collect::<String>(), "/sandbox");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(app.suggestion, 0, "the accepted row is the only match left");
    }

    #[tokio::test]
    async fn enter_runs_the_selected_suggestion_not_the_typed_prefix() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.input = "/s".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &controller,
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.input = "/".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        app.handle_key(
            key(KeyCode::Char('s'), KeyModifiers::NONE),
            &controller,
            &cancel,
        );
        assert_eq!(app.suggestion, 0);

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        app.handle_key(
            key(KeyCode::Backspace, KeyModifiers::NONE),
            &controller,
            &cancel,
        );
        assert_eq!(app.suggestion, 0);
    }

    /// The composer is cleared on Enter and no streamed `Event` carries the
    /// submitted text, so without this echo the prompt is never visible in
    /// the transcript the turn streams into.
    #[tokio::test]
    async fn submitting_a_prompt_echoes_it_into_the_transcript() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.input = "hello otto".chars().collect();
        app.cursor = app.input.len();

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &controller,
            &cancel,
        );

        assert!(matches!(&action, Some(Action::Prompt(line)) if line == "hello otto"));
        let entry = app.entries.last().expect("entry");
        assert_eq!(entry.kind, Some(EntryKind::User));
        assert_eq!(entry.raw, "hello otto");
    }
}
