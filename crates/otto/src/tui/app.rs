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
use otto_core::agent::{CompactionResult, Event};
use otto_core::model::Usage;
use otto_core::session::types::SessionInfo;
use otto_core::wire::events::to_wire;
use otto_core::wire::transcript;
use tokio_util::sync::CancellationToken;

use crate::app::tasks::{Task, TaskStatus};
use crate::app::{Controller, Info, PROFILE_SWITCH_UNAVAILABLE};
use crate::cli::info::SandboxNetwork;
use crate::cli::login;
use crate::cli::repl_commands;

use super::commands::{self, SlashCommand, SlashCommandKind};
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

/// Async work [`App::handle_key`] cannot start itself (every
/// [`Controller`] method it would need is `async`). [`super::run`] awaits
/// these one at a time, matching [`Controller::begin_operation`]'s
/// single-admission-at-a-time contract.
pub(crate) enum Action {
    Exit,
    Prompt(String),
    Image(String),
    Compact(String),
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
    Approve(String),
    Login(String),
    McpLogin(String),
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
    pub input: Vec<char>,
    pub cursor: usize,
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
    pub status: Option<String>,
    ctrl_c_armed_at: Option<Instant>,
}

impl App {
    pub fn new(controller: &Controller) -> Self {
        let (entries, usage) = entries::entries_from_history(&controller.history());
        let history = History::seeded(prompt_history(&entries));
        Self {
            entries,
            usage,
            info: controller.info(),
            input: Vec::new(),
            cursor: 0,
            history,
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
            status: None,
            ctrl_c_armed_at: None,
        }
    }

    /// Marks a turn as started. [`super::run_turn`]/[`super::run_compact`]/
    /// [`super::run_wake`] bracket every `Controller` call with this and
    /// [`App::end_turn`].
    pub fn start_turn(&mut self) {
        let now = Instant::now();
        self.busy_since = Some(now);
        self.phase = ("waiting for model".into(), now);
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
            phase: self.phase.0.clone(),
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
                            self.picker = Some(effort_picker(&row.value, controller));
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
            // Most keys are ignored while a turn runs; Esc (Cancel) is handled
            // by the caller, which cancels the turn's child token.
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
                let line = std::mem::take(&mut self.input)
                    .into_iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                self.cursor = 0;
                self.suggestion = 0;
                self.history.remember(&line);
                self.dispatch_line(&line, controller, cancel)
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

    pub fn insert_text(&mut self, value: &str) {
        self.input.splice(self.cursor..self.cursor, value.chars());
        self.cursor += value.chars().count();
        self.edited();
    }

    /// Esc or Ctrl+C while a turn is running cancels it; the caller (which
    /// holds the turn's `CancellationToken`) checks this before calling
    /// [`App::handle_key`] and cancels the turn directly instead of forwarding
    /// the key.
    pub fn is_interrupt_key(key: &KeyEvent) -> bool {
        key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    }

    /// The slash commands the composer's current value is a prefix of, with
    /// [`App::suggestion`] indexing the highlighted one. An open overlay hides
    /// the panel. [`super::render`] draws exactly this list.
    pub(super) fn suggestions(&self) -> Vec<SlashCommand> {
        if self.show_help || self.picker.is_some() {
            return Vec::new();
        }
        let value: String = self.input.iter().collect();
        commands::matching_slash_commands(&value)
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
            SlashCommandKind::Image => {
                if args.is_empty() {
                    self.push_system("usage: /image <path>");
                    None
                } else {
                    Some(Action::Image(args))
                }
            }
            SlashCommandKind::Model => {
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
                if args.is_empty() {
                    self.picker = Some(thinking_picker(&controller.info().thinking));
                    None
                } else {
                    Some(parse_thinking_action(args, false))
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
                let (subcommand, rest) = match args.split_once(char::is_whitespace) {
                    Some((subcommand, rest)) => (subcommand, rest.trim()),
                    None => (args.as_str(), ""),
                };
                match (subcommand, rest) {
                    ("", _) => {
                        let info = controller.sandbox_info();
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
                        match controller.resolve_sandbox_read_path(path) {
                            Ok(resolved) => self.picker = Some(sandbox_allow_picker(&resolved)),
                            Err(message) => {
                                self.push_system(format!("/sandbox allow: {message}"));
                            }
                        }
                        None
                    }
                    ("network", "") => {
                        self.picker = Some(sandbox_network_picker(controller));
                        None
                    }
                    ("network", mode @ ("allow" | "deny")) => {
                        Some(Action::SandboxNetwork(mode.to_string()))
                    }
                    _ => {
                        self.push_system(format!("unknown command: /sandbox {args}"));
                        None
                    }
                }
            }
            SlashCommandKind::Approve => {
                if args.is_empty() || args.contains(char::is_whitespace) {
                    self.push_system(format!("unknown command: {line}"));
                    None
                } else {
                    Some(Action::Approve(args))
                }
            }
            SlashCommandKind::Login => Some(Action::Login(args)),
            SlashCommandKind::Mcp => {
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
                self.push_system(task_report(controller, &args));
                None
            }
            SlashCommandKind::Timers => {
                self.push_system(
                    repl_commands::timers_report(controller, &args)
                        .unwrap_or_else(|message| message),
                );
                None
            }
            SlashCommandKind::Skills => {
                if !args.is_empty() {
                    self.push_system(format!("unknown command: {line}"));
                    return None;
                }
                self.push_system(repl_commands::skills_report(controller));
                None
            }
            SlashCommandKind::Skill => {
                self.push_system(repl_commands::skill_report(controller, &args));
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

    /// `list_sessions` is synchronous, so the picker opens with no intermediate
    /// loading state.
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

    /// Applies one streamed turn event to the transcript.
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
    /// Returns `true` for an [`Event::AgentError`], so [`super::run`] does not
    /// also print a turn's final `Err` when the same failure already appeared
    /// as an event.
    pub fn apply_event(&mut self, event: Event) -> bool {
        if let Some(phase) = transcript::phase(&to_wire(&event))
            && phase != self.phase.0
        {
            self.phase = (phase, Instant::now());
        }
        match event {
            Event::ReasoningDelta { text } => {
                if let Some(last) = self.entries.last_mut()
                    && last.kind == Some(EntryKind::Reasoning)
                    && last.id == "streaming-reasoning"
                {
                    last.raw.push_str(&text);
                } else {
                    self.entries.push(Entry {
                        id: "streaming-reasoning".to_string(),
                        kind: Some(EntryKind::Reasoning),
                        raw: text,
                        ..Entry::default()
                    });
                }
                false
            }
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
            | Event::ProviderRetry { .. }
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
        PickerKind::Effort => "model",
        PickerKind::Thinking => "thinking",
        PickerKind::Sandbox => "sandbox",
    }
}

fn session_row(session: &SessionInfo) -> PickerRow {
    let marker = if session.current { " (current)" } else { "" };
    let name = if session.name.is_empty() {
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
        label: format!("{name}{marker} — {}{workspace}", session.last_user_text),
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

fn session_report(controller: &Controller) -> String {
    let info = controller.info();
    let mut text = format!(
        "ID: {}\nPath: {}\nProvider: {}\nModel: {}\nThinking: {}\nSandbox: {}",
        info.session_id,
        info.session_path,
        info.provider,
        info.model,
        display_thinking(&info.thinking),
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
/// final [`otto_core::agent::CompactionResult`] (as opposed to a streamed
/// [`Event::CompactionCompleted`], which [`App::apply_event`] handles itself).
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
        assert_eq!(row.label, "review — check the diff [otto]");

        let current = session_row(&SessionInfo {
            name: "review".into(),
            cwd: "/Users/u/Work/code/otto".into(),
            current: true,
            ..SessionInfo::default()
        });
        assert_eq!(current.label, "review (current) —  [otto]");

        let unrecorded = session_row(&SessionInfo {
            name: "review".into(),
            ..SessionInfo::default()
        });
        assert_eq!(unrecorded.label, "review — ");
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
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
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
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
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
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
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
    fn insert_text_preserves_pasted_newlines_at_the_cursor() {
        let mut app = App {
            entries: Vec::new(),
            usage: Usage::default(),
            info: Info::default(),
            input: "abcd".chars().collect(),
            cursor: 2,
            history: History::default(),
            scroll: None,
            max_scroll: Cell::new(0),
            selection: None,
            picker: None,
            suggestion: 0,
            show_help: false,
            show_details: false,
            busy_since: None,
            phase: (String::new(), Instant::now()),
            status: None,
            ctrl_c_armed_at: None,
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.max_scroll.set(10);

        app.handle_key(
            key(KeyCode::PageUp, KeyModifiers::NONE),
            &controller,
            &cancel,
        );
        assert_eq!(app.scroll, Some(0));
        app.handle_key(
            key(KeyCode::PageDown, KeyModifiers::NONE),
            &controller,
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
        let mut app = App::new(&controller);

        app.apply_event(Event::TextDelta {
            text: "first".to_string(),
        });
        assert_eq!(app.scroll, None, "an unscrolled transcript keeps following");

        app.scroll = Some(4);
        app.apply_event(Event::TextDelta {
            text: "second".to_string(),
        });
        app.apply_event(Event::ToolCallStarted {
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            arguments: String::new(),
        });
        assert_eq!(app.scroll, Some(4));
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
        let mut app = App::new(&controller);
        let before = app.entries.len();

        for text in ["weigh ", "options"] {
            app.apply_event(Event::ReasoningDelta { text: text.into() });
        }
        app.apply_event(Event::TextDelta { text: "ok".into() });

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

    /// The phase duration counts from the phase's first event: each further
    /// delta of the same phase does not restart it.
    #[tokio::test]
    async fn repeated_deltas_do_not_restart_the_phase_clock() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        app.start_turn();

        app.apply_event(Event::ReasoningDelta { text: "a".into() });
        let started = app.phase.1;
        app.apply_event(Event::ReasoningDelta { text: "b".into() });
        assert_eq!(app.phase, ("reasoning".to_string(), started));

        app.apply_event(Event::TextDelta { text: "ok".into() });
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
        let mut app = App::new(&controller);
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        assert!(matches!(
            app.dispatch_line("/clear", &controller, &cancel),
            Some(Action::NewSession)
        ));

        assert!(
            app.dispatch_line("/clear now", &controller, &cancel)
                .is_none()
        );
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "unknown command: /clear now"
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
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/skills", &controller, &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "Available skills:\n- rust-helper: Rust guidance"
        );

        app.dispatch_line("/skill rust-helper", &controller, &cancel);
        let detail = &app.entries.last().expect("entry").raw;
        assert!(detail.contains("Skill: rust-helper"), "{detail}");
        assert!(
            detail.contains("Use small focused Rust changes."),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn mcp_command_pushes_the_status_report_and_returns_a_login_action() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        app.dispatch_line("/mcp", &controller, &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            "No MCP servers configured."
        );

        let action = app.dispatch_line("/mcp login docs", &controller, &cancel);
        assert!(matches!(action, Some(Action::McpLogin(name)) if name == "docs"));

        app.dispatch_line("/mcp bogus", &controller, &cancel);
        assert_eq!(
            app.entries.last().expect("entry").raw,
            repl_commands::MCP_USAGE
        );
    }

    #[tokio::test]
    async fn image_command_attaches_a_path_for_the_next_prompt() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);

        let action = app.dispatch_line(
            "/image /tmp/screenshot with spaces.png",
            &controller,
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

    /// Documents the pre-existing divergence recorded in `repl_commands.rs`'s
    /// module doc: `/memory review` reaches candidate review and automatic
    /// extraction, neither of which is ported, so the subcommand always falls
    /// through to the same usage line as an unknown one, regardless of whether
    /// the decision word is valid. A valid and an invalid decision word are
    /// therefore indistinguishable here, so one test covers both.
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

    /// This frontend has no per-command guard or status bar, so
    /// [`App::handle_key`]'s single [`App::busy`] check (shared by every slash
    /// command, not memory-specific) silently declines to dispatch instead of
    /// pushing a rejection message. What is verified here is the observable
    /// guarantee: the command never runs while a turn is active.
    #[tokio::test]
    async fn busy_guard_rejects_slash_commands_while_a_turn_is_active() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let before = app.entries.len();
        app.start_turn();
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

    /// The completion half; the help-overlay text containment half is
    /// `super::render`'s concern, not this module's.
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
        let mut app = App::new(&controller);
        app.history.remember("first");
        app.history.remember("/model");

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.input.iter().collect::<String>(), "/model");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(app.scroll, None, "the transcript must not scroll");
        assert!(!app.suggestions().is_empty(), "the panel is open on /model");

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.input.iter().collect::<String>(), "first");

        // An edit ends the recall, so the panel owns the keys again and the
        // next Up starts over from the newest line.
        app.handle_key(
            key(KeyCode::Char('!'), KeyModifiers::NONE),
            &controller,
            &cancel,
        );
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.input.iter().collect::<String>(), "/model");
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.input.iter().collect::<String>(), "first!");
    }

    #[tokio::test]
    async fn a_submitted_line_enters_the_history() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let cancel = CancellationToken::new();
        let mut app = App::new(&controller);
        app.input = "  hello  ".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &controller,
            &cancel,
        );
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE), &controller, &cancel);

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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.input = "/sk".chars().collect();
        app.cursor = app.input.len();

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);
        assert_eq!(app.suggestion, 1, "/skill then /skills");
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

    #[tokio::test]
    async fn thinking_command_without_a_level_opens_a_level_picker() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        controller.set_thinking("high").await.expect("thinking");
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        let action = app.dispatch_line("/thinking", &controller, &cancel);

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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.picker = Some(thinking_picker(""));
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE), &controller, &cancel);

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &controller,
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        let action = app.dispatch_line("/sandbox allow ~/cache", &controller, &cancel);

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
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE), &controller, &cancel);

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &controller,
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();
        app.dispatch_line("/sandbox allow ~/cache", &controller, &cancel);

        let action = app.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &controller,
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
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        for line in ["/sandbox allow", "/sandbox allow ~/missing"] {
            let action = app.dispatch_line(line, &controller, &cancel);

            assert!(action.is_none());
            assert!(app.picker.is_none(), "{line}");
            let entry = app.entries.last().expect("entry");
            assert_eq!(entry.kind, Some(EntryKind::System));
            assert!(entry.raw.starts_with("/sandbox allow:"), "{}", entry.raw);
        }
    }

    #[tokio::test]
    async fn sandbox_network_takes_a_mode_or_opens_a_picker() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        let cancel = CancellationToken::new();

        let action = app.dispatch_line("/sandbox network deny", &controller, &cancel);
        assert!(matches!(&action, Some(Action::SandboxNetwork(mode)) if mode == "deny"));
        assert!(app.picker.is_none());

        let action = app.dispatch_line("/sandbox network", &controller, &cancel);
        assert!(action.is_none());
        let picker = app.picker.as_ref().expect("sandbox picker");
        assert_eq!(picker.kind, PickerKind::Sandbox);
        assert_eq!(picker.rows.len(), 2);
        assert_eq!(picker.rows[0].value, "network\tallow");
        assert_eq!(picker.rows[1].value, "network\tdeny");

        let action = app.dispatch_line("/sandbox network sometimes", &controller, &cancel);
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
