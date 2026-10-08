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
use crate::{
    app::{Controller, Info},
    subagent::tasks::Task,
};
use crossterm::event::{KeyCode, KeyEvent};
use interaction::composer::{Composer, Edit};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatus {
    pub phase: String,
    pub phase_elapsed: Duration,
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
    Compact(String),
    Reflect(String),
    Login(String),
    McpLogin(String),
    CancelTurn,
}

pub(crate) struct App {
    pub entries: Vec<Entry>,
    pub usage: Usage,
    pub pending_image: Option<otto_core::model::Block>,
    pub info: Info,
    pub composer: Composer,
    pub queued_input: Option<String>,
    /// The newest guidance the active run has queued, painted only from a
    /// back-end read: the TUI never guesses what the run accepted.
    pub queued_steer: Option<otto_core::agent::steering::Message>,
    awaiting_user_echo: Option<String>,
    pub scroll: Option<u16>,
    pub max_scroll: std::cell::Cell<u16>,
    pub selection: Option<Selection>,
    pub show_details: bool,
    pub tasks: Vec<Task>,
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
    requests: Vec<(Ticket, Request)>,
    actions: Vec<Action>,
    busy_since: Option<Instant>,
    phase: (String, Instant),
    retry_event: Option<WireEvent>,
    pub status: Option<String>,
    ctrl_c_armed_at: Option<Instant>,
    skill_names: Vec<String>,
    hidden_approvals: HashSet<String>,
    pub outgoing: Vec<Action>,
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
        let (entries, usage) = entries::entries_from_history(&backend.history());
        let composer = Composer::new(
            entries
                .iter()
                .filter(|e| e.kind == Some(EntryKind::User))
                .map(|e| e.raw.clone())
                .collect(),
        );
        let mut app = Self {
            entries,
            usage,
            pending_image: None,
            info: backend.info(),
            composer,
            queued_input: None,
            queued_steer: None,
            awaiting_user_echo: None,
            scroll: None,
            max_scroll: Default::default(),
            selection: None,
            show_details: false,
            tasks: Vec::new(),
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
            requests: Vec::new(),
            actions: Vec::new(),
            busy_since: None,
            phase: ("".into(), Instant::now()),
            retry_event: None,
            status: None,
            ctrl_c_armed_at: None,
            skill_names: Vec::new(),
            hidden_approvals: HashSet::new(),
            outgoing: Vec::new(),
        };
        app.refresh_tasks(backend);
        app
    }
    pub(crate) fn refresh_tasks(&mut self, backend: &Backend<'_>) {
        if let Backend::Local(c) = backend {
            let info = c.info();
            if (info.workspace.as_str(), info.session_id.as_str())
                != (self.info.workspace.as_str(), self.info.session_id.as_str())
            {
                return;
            }
            self.tasks = c.subagent_tasks().map(|t| t.list()).unwrap_or_default();
            self.skill_names = c.skills().skills().iter().map(|s| s.name.clone()).collect();
            self.skill_names.sort();
        }
    }
    pub fn start_turn(&mut self) {
        self.start_operation("waiting for model");
    }
    pub(crate) fn start_operation(&mut self, phase: &str) {
        let now = Instant::now();
        self.busy_since = Some(now);
        self.phase = (phase.into(), now);
        self.retry_event = None;
    }
    pub fn end_turn(&mut self) {
        self.busy_since = None;
        self.finish_steering();
    }

    /// A turn end is the last moment tracked guidance can be resolved. Guidance
    /// still tracked here never reached the model, so the text goes back into an
    /// empty composer for the user to resend. The loss itself is stated by the
    /// runtime's own `agent_steering` notice: the TUI adds no claim of its own
    /// that could contradict it.
    fn finish_steering(&mut self) {
        let Some(steer) = self.queued_steer.take() else {
            return;
        };
        if self.composer.input.is_empty() {
            self.composer.set_input(&steer.text);
        }
        self.resolve_steering(&steer);
        self.refresh_steering();
    }
    pub(crate) fn has_writes(&self) -> bool {
        !self.writes.is_empty()
    }
    pub fn busy(&self) -> bool {
        self.busy_since.is_some()
    }
    pub fn thinking(&self) -> Option<TurnStatus> {
        Some(TurnStatus {
            phase: self
                .retry_event
                .as_ref()
                .and_then(|e| {
                    transcript::phase_at(
                        e,
                        self.phase.1.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    )
                })
                .unwrap_or_else(|| self.phase.0.clone()),
            phase_elapsed: self.phase.1.elapsed(),
            turn_elapsed: self.busy_since?.elapsed(),
        })
    }
    pub fn refresh(&mut self, backend: &Backend<'_>) {
        let old = (self.info.workspace.clone(), self.info.session_id.clone());
        let (entries, usage) = entries::entries_from_history(&backend.history());
        self.entries = entries;
        self.usage = usage;
        self.info = backend.info();
        self.scroll = None;
        self.invalidate_reads();
        if old != (self.info.workspace.clone(), self.info.session_id.clone()) {
            self.surfaces.clear();
            // The run that owned the tracked guidance ended with the session.
            self.queued_steer = None;
            let draft = self.composer.input.iter().collect::<String>();
            self.composer = Composer::new(
                self.entries
                    .iter()
                    .filter(|e| e.kind == Some(EntryKind::User))
                    .map(|e| e.raw.clone())
                    .collect(),
            );
            self.composer.set_input(&draft);
        }
    }
    pub(crate) fn invalidate_reads(&mut self) {
        self.pending.clear();
        for generation in self.generations.values_mut() {
            *generation = generation.wrapping_add(1);
        }
    }
    pub fn refresh_info(&mut self, backend: &Backend<'_>) {
        let info = backend.info();
        if (info.workspace.as_str(), info.session_id.as_str())
            != (self.info.workspace.as_str(), self.info.session_id.as_str())
        {
            return;
        }
        self.info = info;
        self.usage = self.info.usage;
    }
    pub(crate) fn open_memory_review(&mut self, _controller: &Controller) {
        match self
            .catalog
            .open("/memory review", &self.info, self.attached)
        {
            Ok(transition) => self.transition(self.top_id().unwrap_or(0), transition),
            Err(error) => self.push_system(error),
        }
    }
    pub(crate) fn push_system(&mut self, text: impl Into<String>) {
        self.entries.push(Entry {
            id: format!("system-{}", self.entries.len()),
            kind: Some(EntryKind::System),
            raw: text.into(),
            ..Entry::default()
        });
        self.scroll = None;
    }

    pub(crate) fn has_surface(&self) -> bool {
        self.surfaces.top().is_some()
    }
    pub(crate) fn active_view(&self) -> Option<View> {
        let surface = self.surfaces.top()?;
        let mut view = surface.model.view();
        if let Some(error) = &surface.state.error {
            view.loading = false;
            view.error = Some(error.clone());
            if surface.model.retry().is_some() {
                view.actions.push(interaction::Action {
                    id: "otto:retry".into(),
                    label: "Retry".into(),
                    enabled: self.connected && !surface.state.pending,
                    reason: surface
                        .state
                        .pending
                        .then(|| "operation outcome unknown".into()),
                    ..Default::default()
                });
            }
        }
        if !self.connected {
            for action in &mut view.actions {
                if action.enabled {
                    action.enabled = false;
                    action.reason = Some("TUI disconnected".into());
                }
            }
        }
        if view.document.is_empty()
            && let Some(id) = surface.state.selected_row.as_ref()
            && let Some(row) = view.rows.iter().find(|r| &r.id == id)
        {
            view.document = row.detail.clone();
        }
        Some(view)
    }
    fn top_id(&self) -> Option<u64> {
        self.surfaces.top().map(|s| s.id)
    }
    fn ticket(&mut self, surface: u64, generation: u64) -> Ticket {
        Ticket {
            workspace: self.info.workspace.clone(),
            session: self.info.session_id.clone(),
            surface,
            generation,
        }
    }
    fn enqueue(&mut self, surface: u64, request: Request) {
        // A steering read follows the command that changed the state. Both are
        // serialized by the back end, so the read can never observe a state
        // older than the admission or withdrawal it is meant to show, and the
        // caller's display never flaps back to a stale list.
        let follows_steering = matches!(
            &request,
            Request::Command(
                backend::Command::AdmitInput(_)
                    | backend::Command::Steer(_)
                    | backend::Command::WithdrawSteer(_)
            )
        );
        let generation = self.generations.entry(surface).or_default();
        *generation = generation.wrapping_add(1);
        let g = *generation;
        if surface != 0 {
            self.pending.insert((surface, g));
        }
        if matches!(request, Request::Command(_)) {
            // A steering read takes the command lock only for its ordering: it
            // changes nothing, so it must not hold an ordinary submission back
            // the way an outstanding write does.
            if !matches!(&request, Request::Command(backend::Command::SteeringState)) {
                self.writes.insert((surface, g));
            }
        }
        if matches!(
            &request,
            Request::Command(
                backend::Command::NewSession
                    | backend::Command::Resume(_)
                    | backend::Command::Fork(_)
                    | backend::Command::Archive(_)
                    | backend::Command::SwitchProfile(_)
                    | backend::Command::SwitchProfileThinking { .. }
            )
        ) {
            self.session_changes.insert((surface, g));
        }
        let ticket = self.ticket(surface, g);
        self.pending.insert((surface, g));
        if let Some(driver) = &mut self.driver {
            driver.submit(ticket, request);
        } else {
            self.requests.push((ticket, request));
        }
        if follows_steering {
            self.refresh_steering();
        }
    }

    /// Asks the back end for the active run's guidance. The only source of
    /// [`App::queued_steer`]: an empty answer means nothing is queued any more.
    fn refresh_steering(&mut self) {
        self.enqueue(0, Request::Command(backend::Command::SteeringState));
    }

    /// Paints the tracked guidance from a read. `messages` is the active run's
    /// whole transient lifecycle, oldest first, so the newest queued item is
    /// the one the user can still withdraw.
    fn apply_steering(&mut self, messages: Vec<otto_core::agent::steering::Message>) {
        let queued = messages
            .into_iter()
            .rev()
            .find(|message| message.status == otto_core::agent::steering::Status::Queued);
        if let Some(previous) = self.queued_steer.take()
            && queued
                .as_ref()
                .is_none_or(|message| message.id != previous.id)
        {
            self.resolve_steering(&previous);
        }
        self.queued_steer = queued;
    }

    /// The tracked guidance left the queued state, so its text must not be
    /// mistaken for a later identical message's delivery echo.
    fn resolve_steering(&mut self, steer: &otto_core::agent::steering::Message) {
        if self.awaiting_user_echo.as_deref() == Some(steer.text.as_str()) {
            self.awaiting_user_echo = None;
        }
    }
    fn transition(&mut self, surface: u64, transition: Transition) {
        match transition {
            Transition::Stay => {}
            Transition::Close => {
                let approval = self
                    .surfaces
                    .remove(surface)
                    .is_some_and(|surface| surface.model.key().starts_with("approval:"));
                self.pending.retain(|(id, _)| *id != surface);
                if approval {
                    self.request_approvals();
                }
            }
            Transition::Push(mut presenter) => {
                if let Some(id) = presenter.key().strip_prefix("approval:") {
                    self.hidden_approvals.remove(id);
                }
                let request = presenter.start();
                let view = presenter.view();
                let id = self.surfaces.push(presenter, &view);
                if let Some(request) = request {
                    self.enqueue(id, request)
                } else {
                    self.tick_features();
                }
            }
            Transition::Request(request) => self.enqueue(surface, request),
            Transition::Run(run) => self.actions.push(match run {
                Run::Prompt(s) => Action::Prompt(s),
                Run::Compact(s) => Action::Compact(s),
                Run::Reflect(s) => Action::Reflect(s),
                Run::Login(s) => Action::Login(s),
                Run::McpLogin(s) => Action::McpLogin(s),
                Run::Exit => Action::Exit,
            }),
        }
    }
    pub(crate) fn tick_features(&mut self) {
        let Some(surface) = self.surfaces.top() else {
            return;
        };
        let id = surface.id;
        if !surface.model.periodic() {
            return;
        }
        if self.pending.iter().any(|(s, _)| *s == id) {
            return;
        }
        if let Some(request) = surface.model.refresh() {
            self.enqueue(id, request)
        }
    }
    pub(crate) fn request_approvals(&mut self) {
        if self.pending.iter().any(|(id, _)| *id == 0) {
            return;
        }
        self.enqueue(0, Request::ApprovalSnapshot)
    }
    pub(crate) async fn next_completion(&mut self) -> Completion {
        match self.driver.as_mut() {
            Some(d) => d.next().await,
            None => future::pending().await,
        }
    }
    pub(crate) fn apply_completion(&mut self, completion: Completion) {
        let t = completion.ticket;
        let mutation = completion.mutation;
        let unknown_session = self.session_changes.contains(&(t.surface, t.generation))
            && matches!(&completion.result,Err(error) if error.to_ascii_lowercase().contains("outcome unknown"));
        if !unknown_session {
            self.writes.remove(&(t.surface, t.generation));
            self.session_changes.remove(&(t.surface, t.generation));
        }
        if t.workspace != self.info.workspace || t.session != self.info.session_id {
            if mutation {
                match completion.result {
                    Err(error) => self.push_system(format!(
                        "Previous session operation ({}): {error}",
                        t.session
                    )),
                    Ok(Reply::Operation { message, .. }) if !message.is_empty() => self
                        .push_system(format!(
                            "Previous session operation ({}): {message}",
                            t.session
                        )),
                    Ok(Reply::Management(reply)) => self.push_system(format!(
                        "Previous session operation ({}): {}",
                        t.session,
                        crate::cli::repl_commands::format_management_reply(&reply)
                    )),
                    // A steering read of a session that is no longer open says
                    // nothing the user needs a line for.
                    Ok(Reply::Steering(_)) => {}
                    _ => self.push_system(format!(
                        "An operation completed for previous session {}",
                        t.session
                    )),
                }
            }
            return;
        }
        if let Ok(Reply::Steering(messages)) = &completion.result {
            self.apply_steering(messages.clone());
        }
        if t.surface == 0 {
            self.pending.remove(&(0, t.generation));
            if !mutation && self.generations.get(&0) != Some(&t.generation) {
                return;
            }
            match completion.result {
                Ok(Reply::ApprovalSnapshot(snapshot)) => {
                    self.sync_shared_approval(&snapshot.requests)
                }
                // A steering read has no receipt: it only repaints state.
                Ok(Reply::Steering(_)) => {}
                Ok(reply) if mutation => {
                    if let Some(receipt) = operation_receipt(&reply) {
                        self.apply_receipt(receipt)
                    }
                }
                Err(error) if mutation => self.push_system(error),
                _ => {}
            }
            return;
        }
        self.pending.remove(&(t.surface, t.generation));
        if !mutation && self.generations.get(&t.surface) != Some(&t.generation) {
            return;
        }
        if let Ok(Reply::Image { path, image }) = &completion.result {
            self.pending_image = Some(image.clone());
            self.push_system(format!("Attached image: {path}"));
        }
        let management_notice = if mutation {
            match &completion.result {
                Ok(Reply::Management(reply)) => {
                    Some(crate::cli::repl_commands::format_management_reply(reply))
                }
                _ => None,
            }
        } else {
            None
        };
        let receipt = completion.result.as_ref().ok().and_then(operation_receipt);
        let mut transition = None;
        let mut error = None;
        let mut attached = false;
        if let Some(surface) = self.surfaces.find_mut(t.surface) {
            attached = true;
            surface.state.pending = false;
            match completion.result {
                Ok(reply) => {
                    let had_rows = !surface.model.view().rows.is_empty();
                    transition = Some((surface.id, surface.model.receive(reply)));
                    surface.state.error = None;
                    let view = surface.model.view();
                    if !had_rows && !view.rows.is_empty() && view.default_action.is_none() {
                        surface.state.focus = interaction::Focus::Rows;
                    }
                    surface.state.reconcile(&view);
                }
                Err(message) => {
                    surface.state.pending =
                        mutation && message.to_ascii_lowercase().contains("outcome unknown");
                    surface.state.error = Some(message.clone());
                    error = Some(message);
                }
            }
        } else if let Err(message) = completion.result {
            error = Some(message);
        } else if let Some(message) = management_notice {
            self.push_system(message);
        }
        if let Some(receipt) = receipt {
            self.apply_receipt(receipt);
        }
        if let Some(message) = error
            && (mutation || !attached)
        {
            self.push_system(message);
        }
        if let Some((id, transition)) = transition {
            self.transition(id, transition);
        }
    }
    fn apply_receipt(&mut self, receipt: OperationReceipt) {
        let OperationReceipt {
            message,
            info,
            history,
            prompt,
            draft,
        } = receipt;
        let old = (self.info.workspace.clone(), self.info.session_id.clone());
        self.info = info;
        if let Some(history) = history {
            let frozen: Vec<_> = self
                .session_changes
                .iter()
                .filter(|tag| !self.pending.contains(tag))
                .copied()
                .collect();
            for tag in frozen {
                self.session_changes.remove(&tag);
                self.writes.remove(&tag);
            }
            let (entries, usage) = entries::entries_from_history(&history);
            self.entries = entries;
            self.usage = usage;
        }
        if !message.is_empty() {
            self.push_system(message);
        }
        if old != (self.info.workspace.clone(), self.info.session_id.clone()) {
            self.surfaces.clear();
            self.pending_image = None;
            self.writes.clear();
            self.session_changes.clear();
            // The run that owned the tracked guidance ended with the session.
            self.queued_steer = None;
            self.invalidate_reads();
        }
        if let Some(draft) = draft {
            self.composer.set_input(&draft);
        }
        if let Some(prompt) = prompt {
            self.actions.push(Action::Prompt(prompt));
        }
    }
    pub(crate) fn sync_shared_approval(&mut self, requests: &[crate::approval::Request]) {
        let present: HashSet<_> = requests
            .iter()
            .filter(|r| r.is_human_pending())
            .map(|r| r.id.clone())
            .collect();
        self.hidden_approvals.retain(|id| present.contains(id));
        let covered: HashSet<_> = requests
            .iter()
            .filter(|request| {
                self.surfaces
                    .iter()
                    .any(|surface| surface.model.covers_approval(request))
            })
            .map(|request| request.id.clone())
            .collect();
        let duplicate_surfaces: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| {
                surface
                    .model
                    .key()
                    .strip_prefix("approval:")
                    .is_some_and(|id| covered.contains(id))
            })
            .map(|surface| surface.id)
            .collect();
        for id in duplicate_surfaces {
            self.surfaces.remove(id);
        }
        let existing: HashSet<_> = self
            .surfaces
            .iter()
            .filter_map(|s| s.model.key().strip_prefix("approval:").map(str::to_owned))
            .collect();
        let closed: Vec<_> = existing
            .difference(&present)
            .filter_map(|id| {
                self.surfaces
                    .iter()
                    .find(|s| s.model.key() == format!("approval:{id}"))
                    .map(|s| s.id)
            })
            .collect();
        for id in closed {
            self.surfaces.remove(id);
        }
        for surface in self.surfaces.iter_mut() {
            if surface.model.key().starts_with("approval:") {
                let _ = surface
                    .model
                    .receive(Reply::ApprovalSnapshot(crate::approval::Snapshot {
                        requests: requests.to_vec(),
                        cursor: 0,
                    }));
                surface.state.reconcile(&surface.model.view());
            }
        }
        let approval_open = self
            .surfaces
            .iter()
            .any(|surface| surface.model.key().starts_with("approval:"));
        if !approval_open
            && let Some(request) = requests.iter().find(|r| {
                r.is_human_pending()
                    && !existing.contains(&r.id)
                    && !covered.contains(&r.id)
                    && !self.hidden_approvals.contains(&r.id)
            })
        {
            self.transition(
                0,
                Transition::Push(Box::new(features::Approval::shared(request))),
            );
        }
    }

    fn show_approval(&mut self, presenter: features::Approval) {
        if self
            .surfaces
            .iter()
            .any(|surface| surface.model.key().starts_with("approval:"))
        {
            return;
        }
        self.transition(0, Transition::Push(Box::new(presenter)));
    }

    pub fn dispatch_line(
        &mut self,
        line: &str,
        _backend: &Backend<'_>,
        _cancel: &CancellationToken,
    ) -> Option<Action> {
        if line.starts_with('/') {
            if !self.connected {
                self.push_system("TUI disconnected");
                return None;
            }
            match self.catalog.open(line, &self.info, self.attached) {
                Ok(t) => self.transition(self.top_id().unwrap_or(0), t),
                Err(e) => self.push_system(e),
            }
        } else if self.busy() {
            self.queue_input(line);
        } else {
            self.awaiting_user_echo = Some(line.into());
            if !self.attached {
                self.entries.push(Entry {
                    id: format!("user-{}", self.entries.len()),
                    kind: Some(EntryKind::User),
                    raw: line.into(),
                    ..Default::default()
                });
                self.scroll = None;
            }
            self.actions.push(Action::Prompt(line.into()));
        }
        self.actions.pop()
    }
    fn queue_input(&mut self, line: &str) {
        if self.attached {
            if line.starts_with('/') {
                self.queued_input = Some(line.into())
            } else {
                self.push_system("Queued as the next turn (Ctrl+U withdraws it).");
                self.outgoing.push(Action::Prompt(line.into()));
            }
        } else {
            self.queued_input = Some(line.into());
        }
    }
    pub(crate) fn submit_queued_input(
        &mut self,
        backend: &Backend<'_>,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        if self.has_writes() {
            return None;
        }
        if let Backend::Local(controller) = backend
            && controller.pending_approval().is_some()
            && self
                .queued_input
                .as_ref()
                .is_some_and(|line| !line.starts_with('/'))
        {
            return None;
        }
        let line = self.queued_input.take()?;
        self.dispatch_line(&line, backend, cancel)
    }
    pub(crate) fn submit_input(
        &mut self,
        backend: &Backend<'_>,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        let current = self.composer.input.iter().collect::<String>();
        if self.queued_input.is_some() && !current.trim().starts_with('/') {
            return None;
        }
        let line = self.composer.take()?;
        if !line.starts_with('/') {
            self.awaiting_user_echo = Some(line.clone());
        }
        if self.has_writes() && !line.starts_with('/') {
            self.queued_input = Some(line);
            return None;
        }
        if self.busy() && self.pending_image.is_some() && !line.starts_with('/') {
            self.queued_input = Some(line);
            return None;
        }
        if self.busy() && !line.starts_with('/') {
            // Natural language submitted during an active turn is guidance for
            // that exact turn, never an implicit later queued prompt. The
            // backend resolves the admission race: only an idle result starts
            // a new prompt explicitly.
            self.enqueue(0, Request::Command(backend::Command::AdmitInput(line)));
            return None;
        }
        self.dispatch_line(&line, backend, cancel)
    }
    pub fn insert_text(&mut self, value: &str) {
        if let Some(surface) = self.surfaces.top_mut() {
            let view = surface.model.view();
            if matches!(surface.state.focus, interaction::Focus::Field(_)) {
                surface
                    .state
                    .handle(&view, interaction::Intent::Text(value.into()));
            }
        } else {
            self.composer.insert_text(value)
        }
    }
    fn skill_completions(&self, argument: &str) -> Vec<Suggestion> {
        let fields: Vec<&str> = argument.split_whitespace().collect();
        let ends_in_space = argument.ends_with(char::is_whitespace);
        match (fields.as_slice(), ends_in_space) {
            ([], false) => vec![Suggestion {
                replacement: "/skill".to_string(),
                description: "list skills, show one, or set enabled state".to_string(),
            }],
            ([], true) => self
                .skill_name_completions("")
                .into_iter()
                .chain(std::iter::once(Suggestion {
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

    fn skill_name_completions(&self, prefix: &str) -> Vec<Suggestion> {
        self.skill_names
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| Suggestion {
                replacement: format!("/skill {name}"),
                description: "show skill details".to_string(),
            })
            .collect()
    }

    fn skill_name_completions_for_set(&self, prefix: &str) -> Vec<Suggestion> {
        self.skill_names
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| Suggestion {
                replacement: format!("/skill set {name}"),
                description: "choose a skill to set".to_string(),
            })
            .collect()
    }

    pub(crate) fn suggestions(&self) -> Vec<super::commands::Completion> {
        let line = self.composer.input.iter().collect::<String>();
        if self.has_surface() {
            return vec![];
        }
        if !line.starts_with('/') {
            return vec![];
        }
        if line.contains(['\r', '\n']) {
            return Vec::new();
        }
        if let Some(argument) = line.strip_prefix("/skill") {
            return self.skill_completions(argument);
        }
        let prefix = line.as_str();
        self.catalog
            .entries()
            .iter()
            .filter(|entry| entry.name.starts_with(prefix))
            .map(|entry| super::commands::Completion {
                replacement: entry.name.into(),
                description: entry.description.into(),
            })
            .collect()
    }
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        backend: &Backend<'_>,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        self.selection = None;
        if super::input::is_ctrl_c(key) {
            if self.busy() {
                return Some(Action::CancelTurn);
            }
            let now = Instant::now();
            if self
                .ctrl_c_armed_at
                .is_some_and(|at| now < at + CTRL_C_ARM_WINDOW)
            {
                return Some(Action::Exit);
            }
            self.ctrl_c_armed_at = Some(now);
            self.status = Some(CTRL_C_EXIT_STATUS.into());
            self.composer.edit(Edit::Clear);
            return None;
        }
        self.ctrl_c_armed_at = None;
        if self.status.as_deref() == Some(CTRL_C_EXIT_STATUS) {
            self.status = None
        }
        if self.top_id().is_some() {
            {
                let geometry = self.geometry.borrow();
                let state = &mut self.surfaces.top_mut()?.state;
                state.page_rows = geometry.page_rows.max(1);
                state.scroll = state.scroll.min(geometry.max_scroll);
            }
            let first = self.active_view().unwrap();
            self.surfaces.top_mut()?.state.reconcile(&first);
            let view = self.active_view().unwrap();
            let intent = super::input::surface_intent(key, &self.surfaces.top().unwrap().state)?;
            let activation = self.surfaces.top_mut()?.state.handle(&view, intent);
            self.surfaces.top_mut()?.state.scroll = self
                .surfaces
                .top()?
                .state
                .scroll
                .min(self.geometry.borrow().max_scroll);
            if let Some(a) = activation {
                if a == Activation::Back {
                    if let Some(id) = self
                        .surfaces
                        .top()
                        .unwrap()
                        .model
                        .key()
                        .strip_prefix("approval:")
                    {
                        self.hidden_approvals.insert(id.to_string());
                    }
                    self.surfaces.pop();
                    return None;
                }
                let surface = self.surfaces.top_mut()?;
                let sid = surface.id;
                if matches!(&a,Activation::Action{id,..} if id=="otto:retry") {
                    let request = surface.model.retry();
                    if let Some(request) = request {
                        surface.state.pending = false;
                        surface.state.error = None;
                        self.transition(sid, Transition::Request(request));
                    }
                    return None;
                }
                let tr = surface.model.activate(a);
                let updated_view = surface.model.view();
                surface.state.reconcile(&updated_view);
                if matches!(tr, Transition::Request(Request::Command(_))) {
                    surface.state.pending = true;
                }
                if !self.connected && matches!(tr, Transition::Request(Request::Command(_))) {
                    self.surfaces.top_mut()?.state.pending = false;
                    self.surfaces.top_mut()?.state.error = Some("TUI disconnected".into());
                } else {
                    self.transition(sid, tr);
                }
            }
            return self.actions.pop();
        }
        if super::input::is_toggle_details(key) {
            self.show_details = !self.show_details;
            return None;
        }
        if super::input::is_help(key) && self.composer.input.is_empty() {
            return self.dispatch_line("/help", backend, cancel);
        }
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            self.handle_scroll_key(&key);
            return None;
        }
        let suggestions = self.suggestions();
        if !suggestions.is_empty() {
            let selected = self.composer.suggestion.min(suggestions.len() - 1);
            match key.code {
                KeyCode::Up | KeyCode::Down if !self.composer.recalling() => {
                    let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                    self.composer.suggestion =
                        (selected as isize + delta).rem_euclid(suggestions.len() as isize) as usize;
                    return None;
                }
                KeyCode::Tab => {
                    self.composer.set_input(&suggestions[selected].replacement);
                    return None;
                }
                KeyCode::Enter if super::input::is_submit(key) => {
                    self.composer.set_input(&suggestions[selected].replacement)
                }
                _ => {}
            }
        }
        if super::input::is_submit(key) {
            return self.submit_input(backend, cancel);
        }
        if super::input::is_newline(key) {
            self.composer.edit(Edit::Newline);
            return None;
        }
        if self.busy()
            || self.has_writes()
            || self.queued_input.is_some()
            || self.queued_steer.is_some()
        {
            if self.busy() && key.code == KeyCode::Esc {
                return Some(Action::CancelTurn);
            }
            if key.code == KeyCode::Char('u')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                // The run owns the guidance's state, so the withdrawal goes to
                // it and the read that follows repaints the display: dropping
                // it locally could hide a message that is still waiting.
                if let Some(steer) = &self.queued_steer {
                    self.enqueue(
                        0,
                        Request::Command(backend::Command::WithdrawSteer(steer.id)),
                    );
                } else if self.queued_input.is_some() {
                    self.queued_input = None;
                    self.awaiting_user_echo = None;
                }
                self.composer.edit(Edit::Clear);
                return None;
            }
        }
        if let Some(edit) = super::input::composer_edit(key) {
            self.composer.edit(edit)
        }
        self.actions.pop()
    }
    pub(crate) fn drain_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }
    pub fn scroll_wheel(&mut self, up: bool) {
        if self.has_surface() {
            let view = self.active_view().unwrap();
            let geometry = self.geometry.borrow();
            if let Some(surface) = self.surfaces.top_mut() {
                surface.state.scroll = surface.state.scroll.min(geometry.max_scroll);
                surface
                    .state
                    .handle(&view, interaction::Intent::Move(if up { -1 } else { 1 }));
                surface.state.scroll = surface.state.scroll.min(geometry.max_scroll);
            }
            return;
        }
        if up {
            self.scroll = self
                .scroll
                .map_or(Some(self.max_scroll.get().saturating_sub(1)), |x| {
                    Some(x.saturating_sub(1))
                })
        } else if let Some(x) = self.scroll {
            let next = x.saturating_add(1);
            self.scroll = (next < self.max_scroll.get()).then_some(next)
        }
    }
    pub fn handle_scroll_key(&mut self, key: &KeyEvent) -> bool {
        let amount = match key.code {
            KeyCode::Up => 1,
            KeyCode::Down => 1,
            KeyCode::PageUp => 10,
            KeyCode::PageDown => 10,
            _ => return false,
        };
        if self.has_surface() {
            return false;
        }
        if matches!(key.code, KeyCode::Up | KeyCode::PageUp) {
            let top = self.scroll.unwrap_or(self.max_scroll.get());
            self.scroll = Some(top.saturating_sub(amount))
        } else if let Some(top) = self.scroll {
            let next = top.saturating_add(amount);
            self.scroll = (next < self.max_scroll.get()).then_some(next)
        }
        true
    }

    pub fn apply_event(&mut self, event: &WireEvent) -> bool {
        if let Some(phase) = transcript::phase(event)
            && phase != self.phase.0
        {
            self.phase = (phase, Instant::now());
            self.retry_event = (event.event_type == "provider_retry").then(|| event.clone());
        }
        match event.event_type.as_str() {
            "provider_retry"
                if event
                    .retry
                    .as_ref()
                    .is_some_and(|retry| retry.interrupted_output) =>
            {
                self.push_system(transcript::INTERRUPTED_OUTPUT_NOTICE);
            }
            "plan_updated" => {
                if let Some(steps) = &event.steps {
                    self.info.plan = steps.clone();
                }
            }
            "reasoning_delta" => append_stream(
                &mut self.entries,
                "streaming-reasoning",
                EntryKind::Reasoning,
                &event.text,
            ),
            "text_delta" => append_stream(
                &mut self.entries,
                "streaming",
                EntryKind::Assistant,
                &event.text,
            ),
            "provider_usage" if event.usage_present == Some(true) => {
                if let Some(usage) = event.usage {
                    self.usage.input_tokens = self
                        .usage
                        .input_tokens
                        .saturating_add(usage.input_tokens.max(0));
                    self.usage.output_tokens = self
                        .usage
                        .output_tokens
                        .saturating_add(usage.output_tokens.max(0));
                    self.usage.cached_input_tokens = self
                        .usage
                        .cached_input_tokens
                        .saturating_add(usage.cached_input_tokens.max(0));
                }
            }
            "tool_call_started" => self.entries.push(Entry {
                id: format!("streaming-tool-{}", self.entries.len()),
                kind: Some(EntryKind::Tool),
                tool_call_id: event.tool_call_id.clone(),
                tool_name: event.tool_name.clone(),
                tool_args: event
                    .tool_args
                    .as_deref()
                    .map(|v| v.get().to_owned())
                    .unwrap_or_default(),
                operation_id: Some(event.operation_id.clone()),
                ..Entry::default()
            }),
            "tool_call_finished" => {
                let result = event.result.clone().unwrap_or_default();
                let approval = if !self.attached {
                    let parsed = ToolResult {
                        content: result.content.clone(),
                        is_error: result.is_error,
                        ..ToolResult::default()
                    };
                    crate::tool::bash::parse_approval_request(&event.tool_name, &parsed)
                } else {
                    None
                };
                if let Some(e) = self.entries.iter_mut().rev().find(|e| {
                    e.kind == Some(EntryKind::Tool) && e.tool_call_id == event.tool_call_id
                }) {
                    e.tool_output = result.content;
                    e.tool_error = result.is_error;
                    e.tool_done = true;
                    e.operation_id = Some(event.operation_id.clone());
                    e.disposition = result.disposition;
                    e.effect_certainty = result.effect_certainty;
                    e.stop_reason = result.stop_reason;
                }
                if let Some(request) = approval {
                    self.show_approval(features::Approval::legacy(
                        request.id,
                        request.command,
                        request.justification,
                        request.read_path,
                    ));
                }
            }
            "compaction_completed" => {
                if let Some(c) = &event.compaction {
                    self.push_system(compaction_line(c))
                }
            }
            "compaction_warning" | "memory_warning" => self.push_system(event.error.clone()),
            "agent_error" => {
                self.push_system(event.error.clone());
                return true;
            }
            USER_MESSAGE => {
                // The run appended this text as the user's message, which is its
                // own word that queued guidance reached the model. The steer is
                // resolved here so a turn end cannot offer it again.
                if let Some(steer) = self
                    .queued_steer
                    .as_ref()
                    .filter(|steer| steer.text == event.text)
                    .cloned()
                {
                    self.queued_steer = None;
                    self.resolve_steering(&steer);
                }
                if self.attached {
                    if self.awaiting_user_echo.as_deref() == Some(event.text.as_str()) {
                        self.awaiting_user_echo = None;
                    }
                    self.entries.push(Entry {
                        id: format!("user-{}", self.entries.len()),
                        kind: Some(EntryKind::User),
                        raw: event.text.clone(),
                        ..Entry::default()
                    })
                } else {
                    let queued = self.queued_input.take();
                    let expected_echo =
                        self.awaiting_user_echo.as_deref() == Some(event.text.as_str());
                    let duplicate = queued.is_none()
                        && self.entries.last().is_some_and(|entry| {
                            entry.kind == Some(EntryKind::User) && entry.raw == event.text
                        });
                    let line = queued
                        .as_ref()
                        .cloned()
                        .unwrap_or_else(|| event.text.clone());
                    if queued.is_none() && !expected_echo {
                        self.composer.remember(&line);
                    }
                    if expected_echo {
                        self.awaiting_user_echo = None;
                    }
                    if !duplicate {
                        self.entries.push(Entry {
                            id: format!("user-{}", self.entries.len()),
                            kind: Some(EntryKind::User),
                            raw: line,
                            ..Entry::default()
                        });
                    }
                }
            }
            "notification" => self.push_system(format!("[task {}] {}", event.task_id, event.text)),
            APPROVAL_REQUESTED => {
                if let Some(req) = &event.approval {
                    if req.is_human_pending() {
                        self.show_approval(features::Approval::shared(req));
                    }
                } else {
                    self.show_approval(features::Approval::legacy(
                        event.approval_id.clone(),
                        event.command.clone(),
                        event.justification.clone(),
                        event.read_path.clone(),
                    ));
                }
            }
            APPROVAL_DECIDED => {
                let key = format!("approval:{}", event.approval_id);
                let ids: Vec<_> = self
                    .surfaces
                    .iter()
                    .filter(|s| s.model.key() == key)
                    .map(|s| s.id)
                    .collect();
                for id in ids {
                    self.surfaces.remove(id);
                }
                self.push_system(format!(
                    "Approval {} decided elsewhere: {}",
                    event.approval_id, event.decision
                ));
            }
            "approval_changed" => {
                if let Some(request) = &event.approval {
                    if request
                        .decision
                        .as_ref()
                        .is_some_and(|d| d.transport == "auto-mode")
                    {
                        self.push_system(format!(
                            "[approval] {} ({}): automatically approved · application {:?}\n{}{}",
                            request.title,
                            request.id,
                            request.application,
                            request.subject.details,
                            if request.application_error.is_empty() {
                                String::new()
                            } else {
                                format!("\n{}", request.application_error)
                            }
                        ));
                    }
                    if let Some(decision) = &request.decision {
                        self.push_system(format!(
                            "[approval] {} ({}) {:?} · {}\n{}",
                            request.title,
                            request.id,
                            request.state,
                            decision.transport,
                            decision.feedback
                        ));
                    }
                    if request.state == crate::approval::State::Pending
                        && !request.is_human_pending()
                    {
                        self.push_system(format!(
                            "[approval] reviewing: {} ({})\n{}\nReason: {}",
                            request.title, request.id, request.subject.details, request.reason
                        ));
                    }
                    if request.is_human_pending()
                        && !self.hidden_approvals.contains(&request.id)
                        && !self
                            .surfaces
                            .iter()
                            .any(|s| s.model.key() == format!("approval:{}", request.id))
                    {
                        self.show_approval(features::Approval::shared(request));
                    } else if !request.is_human_pending() {
                        let key = format!("approval:{}", request.id);
                        let ids: Vec<_> = self
                            .surfaces
                            .iter()
                            .filter(|s| s.model.key() == key)
                            .map(|s| s.id)
                            .collect();
                        for id in ids {
                            self.surfaces.remove(id);
                        }
                    }
                }
            }
            _ => {}
        }
        false
    }
}

fn append_stream(entries: &mut Vec<Entry>, id: &str, kind: EntryKind, text: &str) {
    if let Some(last) = entries
        .last_mut()
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
    let before = super::layout::format_token_count(result.tokens_before);
    if result.estimated_tokens_after > 0 {
        format!(
            "[context] compacted {before} → {} tokens",
            super::layout::format_token_count(result.estimated_tokens_after)
        )
    } else {
        format!("[context] compacted {before} tokens")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::interaction::{Action as SurfaceAction, Field, Row};
    use otto_core::agent::steering::{Message as SteerMessage, Status};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Probe {
        key: &'static str,
        view: View,
        received: Arc<AtomicUsize>,
    }
    impl Presenter for Probe {
        fn key(&self) -> String {
            self.key.into()
        }
        fn view(&self) -> View {
            self.view.clone()
        }
        fn activate(&mut self, _: Activation) -> Transition {
            Transition::Stay
        }
        fn receive(&mut self, _: Reply) -> Transition {
            self.received.fetch_add(1, Ordering::SeqCst);
            Transition::Stay
        }
    }
    fn app() -> (App, Remote) {
        let remote = Remote::offline(Vec::new());
        let app = App::new(&Backend::Attach(&remote));
        (app, remote)
    }
    #[test]
    fn provider_recovery_separates_partial_output_in_local_and_attached_tui() {
        for attached in [false, true] {
            let (mut app, _remote) = app();
            app.attached = attached;
            for json in [
                r#"{"type":"text_delta","text":"partial"}"#,
                r#"{"type":"provider_retry","retry":{"attempt":2,"max_attempts":7,"interrupted_output":true}}"#,
                r#"{"type":"text_delta","text":"replacement"}"#,
            ] {
                app.apply_event(&serde_json::from_str(json).unwrap());
            }
            let entries = &app.entries[app.entries.len() - 3..];
            assert_eq!(entries[0].raw, "partial");
            assert_eq!(entries[1].kind, Some(EntryKind::System));
            assert_eq!(entries[1].raw, transcript::INTERRUPTED_OUTPUT_NOTICE);
            assert_eq!(entries[2].raw, "replacement");
        }
    }

    fn probe(key: &'static str, view: View, received: Arc<AtomicUsize>) -> Box<dyn Presenter> {
        Box::new(Probe {
            key,
            view,
            received,
        })
    }
    fn approval(id: &str) -> crate::approval::Request {
        crate::approval::Request {
            options: vec![],
            schema_version: 1,
            id: id.into(),
            revision: 1,
            kind: crate::approval::Kind::Confirmation,
            authority: crate::approval::Authority::Consent,
            scope: crate::approval::Scope {
                workspace: "/work".into(),
                session_id: "session".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            presentation_origin: String::new(),
            subject: crate::approval::Subject {
                tool_call_id: String::new(),
                source: "test".into(),
                id: id.into(),
                version: "1".into(),
                details: "details".into(),
            },
            title: "Confirm".into(),
            reason: String::new(),
            persistent: false,
            blocking: true,
            created_at: 1,
            expires_at: None,
            state: crate::approval::State::Pending,
            decision: None,
            application: crate::approval::ApplicationState::NotRequired,
            application_error: String::new(),
            review: None,
        }
    }
    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn back_pops_nested_surface_and_keeps_parent_selection_and_draft() {
        let (mut app, remote) = app();
        let marker = Arc::new(AtomicUsize::new(0));
        let view = View {
            rows: vec![
                Row {
                    id: "a".into(),
                    ..Default::default()
                },
                Row {
                    id: "b".into(),
                    ..Default::default()
                },
            ],
            fields: vec![Field {
                id: "feedback".into(),
                max_bytes: 50,
                ..Default::default()
            }],
            ..Default::default()
        };
        app.transition(0, Transition::Push(probe("parent", view, marker.clone())));
        let id = app.surfaces.top().unwrap().id;
        {
            let surface = app.surfaces.top_mut().unwrap();
            surface
                .state
                .handle(&surface.model.view(), interaction::Intent::Move(1));
            surface
                .state
                .drafts
                .insert("feedback".into(), "keep me".into());
        }
        let selected = app.surfaces.top().unwrap().state.selected_row.clone();
        app.transition(
            id,
            Transition::Push(probe("child", View::default(), marker)),
        );
        app.handle_key(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &Backend::Attach(&remote),
            &CancellationToken::new(),
        );
        let parent = app.surfaces.top().unwrap();
        assert_eq!(parent.model.key(), "parent");
        assert_eq!(parent.state.selected_row, selected);
        assert_eq!(parent.state.drafts["feedback"], "keep me");
    }

    #[test]
    fn dismissed_approval_stays_hidden_until_the_collection_reopens_it() {
        let (mut app, _) = app();
        let request = approval("a1");
        app.hidden_approvals.insert("a1".into());
        app.sync_shared_approval(std::slice::from_ref(&request));
        assert!(!app.has_surface());
        app.transition(
            0,
            Transition::Push(Box::new(features::Approval::shared(&request))),
        );
        assert!(app.has_surface());
        assert!(!app.hidden_approvals.contains("a1"));
    }

    #[test]
    fn pending_approvals_are_presented_one_at_a_time() {
        let (mut app, _) = app();
        let first = approval("a1");
        let second = approval("a2");

        app.sync_shared_approval(&[first.clone(), second.clone()]);
        let visible: Vec<_> = app
            .surfaces
            .iter()
            .filter(|surface| surface.model.key().starts_with("approval:"))
            .map(|surface| surface.model.key())
            .collect();
        assert_eq!(visible, ["approval:a1"]);

        app.sync_shared_approval(&[second]);
        let visible: Vec<_> = app
            .surfaces
            .iter()
            .filter(|surface| surface.model.key().starts_with("approval:"))
            .map(|surface| surface.model.key())
            .collect();
        assert_eq!(visible, ["approval:a2"]);
    }

    #[test]
    fn completing_the_shared_poll_allows_the_next_poll() {
        let (mut app, _) = app();
        app.request_approvals();
        let first = app.requests.last().unwrap().0.clone();
        app.apply_completion(Completion {
            mutation: false,
            ticket: first,
            result: Ok(Reply::ApprovalSnapshot(crate::approval::Snapshot {
                requests: vec![],
                cursor: 0,
            })),
        });
        app.request_approvals();
        assert_eq!(app.requests.len(), 2);
        assert!(app.requests[1].0.generation > app.requests[0].0.generation);
    }

    #[test]
    fn obsolete_read_results_are_ignored_after_filter_or_session_changes() {
        let (mut app, _) = app();
        let received = Arc::new(AtomicUsize::new(0));
        app.transition(
            0,
            Transition::Push(probe("reader", View::default(), received.clone())),
        );
        let id = app.surfaces.top().unwrap().id;
        app.enqueue(id, Request::Sessions);
        let old = app.requests.last().unwrap().0.clone();
        app.enqueue(id, Request::Profiles);
        app.apply_completion(Completion {
            mutation: false,
            ticket: old.clone(),
            result: Ok(Reply::Sessions(vec![])),
        });
        assert_eq!(received.load(Ordering::SeqCst), 0);
        app.info.session_id = "changed".into();
        let latest = app.requests.last().unwrap().0.clone();
        app.apply_completion(Completion {
            mutation: false,
            ticket: latest,
            result: Ok(Reply::Profiles(vec![])),
        });
        assert_eq!(received.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dismissed_mutation_keeps_its_receipt_and_retry_prompt() {
        let (mut app, _) = app();
        let received = Arc::new(AtomicUsize::new(0));
        app.transition(
            0,
            Transition::Push(probe("writer", View::default(), received)),
        );
        let id = app.surfaces.top().unwrap().id;
        app.enqueue(
            id,
            Request::Command(backend::Command::Rename("name".into())),
        );
        let ticket = app.requests.last().unwrap().0.clone();
        app.surfaces.remove(id);
        let info = app.info.clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket,
            result: Ok(Reply::Operation {
                message: "change applied".into(),
                info,
                history: None,
                prompt: Some("retry this".into()),
            }),
        });
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.raw == "change applied")
        );
        assert!(
            app.drain_actions()
                .contains(&Action::Prompt("retry this".into()))
        );
    }

    #[test]
    fn a_fork_puts_its_prompt_in_the_editor_without_sending_it() {
        let (mut app, _) = app();
        app.enqueue(
            0,
            Request::Command(backend::Command::Fork("00000002".into())),
        );
        let ticket = app.requests.last().unwrap().0.clone();
        let mut info = app.info.clone();
        info.session_id = "forked".into();
        app.apply_completion(Completion {
            mutation: true,
            ticket,
            result: Ok(Reply::Forked {
                message: "Forked: forked".into(),
                info,
                history: Vec::new(),
                draft: Some("second".into()),
            }),
        });
        assert_eq!(app.info.session_id, "forked");
        assert_eq!(app.composer.input.iter().collect::<String>(), "second");
        assert!(
            !app.drain_actions()
                .iter()
                .any(|action| matches!(action, Action::Prompt(_)))
        );
    }

    #[test]
    fn user_message_echo_does_not_duplicate_prompt_history() {
        let (mut app, remote) = app();
        app.insert_text("hello");
        assert!(
            matches!(app.handle_key(enter(),&Backend::Attach(&remote),&CancellationToken::new()),Some(Action::Prompt(line)) if line=="hello")
        );
        app.apply_event(&WireEvent {
            event_type: USER_MESSAGE.into(),
            text: "hello".into(),
            ..Default::default()
        });
        app.composer.edit(Edit::PreviousHistory);
        assert_eq!(app.composer.input.iter().collect::<String>(), "hello");
        app.composer.edit(Edit::NextHistory);
        assert!(app.composer.input.is_empty());
        app.insert_text("queued");
        let line = app.composer.take().unwrap();
        app.queued_input = Some(line.clone());
        app.awaiting_user_echo = Some(line.clone());
        app.apply_event(&WireEvent {
            event_type: USER_MESSAGE.into(),
            text: line.clone(),
            ..Default::default()
        });
        app.composer.edit(Edit::PreviousHistory);
        assert_eq!(app.composer.input.iter().collect::<String>(), line);
    }

    #[test]
    fn provider_usage_updates_the_footer_during_a_turn() {
        let (mut app, _) = app();
        app.usage = Usage {
            input_tokens: 10,
            output_tokens: 2,
            cached_input_tokens: 4,
        };

        app.apply_event(&WireEvent {
            event_type: "provider_usage".into(),
            usage: Some(Usage {
                input_tokens: 7,
                output_tokens: 3,
                cached_input_tokens: 5,
            }),
            usage_present: Some(true),
            ..Default::default()
        });
        app.apply_event(&WireEvent {
            event_type: "provider_usage".into(),
            usage: Some(Usage::default()),
            usage_present: Some(false),
            ..Default::default()
        });

        assert_eq!(
            app.usage,
            Usage {
                input_tokens: 17,
                output_tokens: 5,
                cached_input_tokens: 9,
            }
        );
    }

    #[test]
    fn paste_edits_focused_surface_feedback_only() {
        let (mut app, _) = app();
        let view = View {
            fields: vec![Field {
                id: "feedback".into(),
                max_bytes: 100,
                ..Default::default()
            }],
            ..Default::default()
        };
        app.transition(
            0,
            Transition::Push(probe("approval:a1", view, Arc::new(AtomicUsize::new(0)))),
        );
        {
            let state = &mut app.surfaces.top_mut().unwrap().state;
            state.focus = interaction::Focus::Field("feedback".into());
        }
        app.insert_text("limit it");
        assert!(app.composer.input.is_empty());
        assert_eq!(
            app.surfaces.top().unwrap().state.drafts["feedback"],
            "limit it"
        );
    }

    #[test]
    fn session_replacement_clears_surfaces_and_reloads_history_while_preserving_draft() {
        let (mut app, remote) = app();
        app.info.session_id = "old".into();
        app.insert_text("draft");
        app.transition(
            0,
            Transition::Push(probe(
                "old-surface",
                View::default(),
                Arc::new(AtomicUsize::new(0)),
            )),
        );
        app.refresh(&Backend::Attach(&remote));
        assert!(!app.has_surface());
        assert!(app.entries.is_empty());
        assert_eq!(app.info.session_id, "");
        assert_eq!(app.composer.input.iter().collect::<String>(), "draft");
    }

    #[test]
    fn unknown_mutation_outcome_keeps_submission_locked() {
        let (mut app, _) = app();
        let view = View {
            actions: vec![SurfaceAction {
                id: "save".into(),
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        app.transition(
            0,
            Transition::Push(probe("writer", view, Arc::new(AtomicUsize::new(0)))),
        );
        let id = app.surfaces.top().unwrap().id;
        app.enqueue(
            id,
            Request::Command(backend::Command::Rename("name".into())),
        );
        let ticket = app.requests.last().unwrap().0.clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket,
            result: Err("operation outcome unknown; reconcile state before retrying".into()),
        });
        assert!(app.surfaces.top().unwrap().state.pending);
        assert!(
            app.active_view()
                .unwrap()
                .actions
                .iter()
                .find(|a| a.id == "otto:retry")
                .is_none_or(|a| !a.enabled)
        );
        let view = app.active_view().unwrap();
        assert_eq!(
            app.surfaces
                .top_mut()
                .unwrap()
                .state
                .handle(&view, interaction::Intent::Activate),
            None
        );
    }
    #[test]
    fn command_arguments_survive_completion_and_pending_mutation_defers_plain_input() {
        let (mut app, remote) = app();
        app.attached = false;
        app.composer.set_input("/thinking high --save");
        assert!(app.suggestions().is_empty());
        app.handle_key(
            KeyEvent::from(KeyCode::Enter),
            &Backend::Attach(&remote),
            &CancellationToken::new(),
        );
        assert!(
            matches!(&app.requests.last().unwrap().1,Request::Command(backend::Command::SetThinking{thinking,save:true}) if thinking=="high")
        );
        let ticket = app.requests.last().unwrap().0.clone();
        app.surfaces.clear();
        app.composer.set_input("after configuration");
        assert_eq!(
            app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
            None
        );
        assert_eq!(app.queued_input.as_deref(), Some("after configuration"));
        app.apply_completion(Completion {
            mutation: true,
            ticket,
            result: Ok(Reply::Operation {
                message: "Applied".into(),
                info: app.info.clone(),
                history: None,
                prompt: None,
            }),
        });
        assert!(!app.has_writes());
        assert_eq!(
            app.submit_queued_input(&Backend::Attach(&remote), &CancellationToken::new()),
            Some(Action::Prompt("after configuration".into()))
        );
    }
    #[test]
    fn reviewing_approval_never_opens_an_approval_surface() {
        let (mut app, _) = app();
        let mut request = approval("reviewing");
        request.review = Some(crate::approval::ReviewMetadata {
            status: crate::approval::ReviewStatus::Reviewing,
            assessment: None,
            settings_revision: 1,
            evidence_digest: "evidence".into(),
            policy_digest: "policy".into(),
        });
        app.sync_shared_approval(&[request]);
        assert_eq!(app.surfaces.iter().count(), 0);
    }

    #[test]
    fn memory_review_surface_consumes_its_approval_without_an_extra_dialog() {
        let (mut app, remote) = app();
        app.attached = false;
        app.dispatch_line(
            "/memory review",
            &Backend::Attach(&remote),
            &CancellationToken::new(),
        );
        let mut request = approval("memory-1");
        request.subject.source = "memory".into();
        app.sync_shared_approval(&[request]);
        assert_eq!(app.surfaces.iter().count(), 1);
        assert_eq!(app.surfaces.top().unwrap().model.key(), "reviews");
    }
    #[test]
    fn scroll_reverses_immediately_after_reaching_the_document_end() {
        let (mut app, _) = app();
        app.transition(
            0,
            Transition::Push(probe(
                "document",
                View {
                    document: "one\ntwo\nthree".into(),
                    ..Default::default()
                },
                Arc::new(AtomicUsize::new(0)),
            )),
        );
        app.geometry.borrow_mut().max_scroll = 2;
        app.surfaces.top_mut().unwrap().state.scroll = u16::MAX;
        app.scroll_wheel(true);
        assert_eq!(app.surfaces.top().unwrap().state.scroll, 1);
    }

    fn steer(id: u64, text: &str, status: otto_core::agent::steering::Status) -> SteerMessage {
        SteerMessage {
            id,
            text: text.into(),
            status,
            input_id: String::new(),
        }
    }

    fn control_u() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('u'), crossterm::event::KeyModifiers::CONTROL)
    }

    fn commands(app: &App) -> Vec<Request> {
        app.requests
            .iter()
            .map(|(_, request)| request.clone())
            .collect()
    }

    /// The run's own read is the only source of the queued guidance: a message
    /// that left the queued state disappears without the TUI deciding anything.
    #[test]
    fn a_steering_read_paints_and_clears_the_tracked_guidance() {
        let (mut app, _) = app();
        app.apply_steering(vec![steer(1, "use the other branch", Status::Queued)]);
        assert_eq!(app.queued_steer.as_ref().map(|steer| steer.id), Some(1));

        // Newest queued wins, and a delivered earlier message is history.
        app.apply_steering(vec![
            steer(1, "first", Status::Delivered),
            steer(2, "second", Status::Queued),
        ]);
        assert_eq!(
            app.queued_steer.as_ref().map(|steer| steer.text.as_str()),
            Some("second")
        );

        app.apply_steering(vec![steer(2, "second", Status::Delivered)]);
        assert!(app.queued_steer.is_none());
    }

    /// A submission during a busy turn must be admitted, and its guidance must
    /// show up promptly: the read follows the admission on the command lock.
    #[test]
    fn a_busy_submission_admits_guidance_then_reads_the_state_after_it() {
        let (mut app, remote) = app();
        app.start_turn();
        app.composer.set_input("use the other branch");
        assert_eq!(
            app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
            None
        );
        assert!(matches!(
            commands(&app).as_slice(),
            [Request::Command(backend::Command::AdmitInput(text)),
             Request::Command(backend::Command::SteeringState)] if text == "use the other branch"
        ));
        let (admission, _) = app.requests[0].clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket: admission,
            result: Ok(Reply::Operation {
                message: "Guidance queued for the active task: use the other branch".into(),
                info: app.info.clone(),
                history: None,
                prompt: None,
            }),
        });
        let (read, _) = app.requests[1].clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket: read,
            result: Ok(Reply::Steering(vec![steer(
                1,
                "use the other branch",
                Status::Queued,
            )])),
        });
        assert_eq!(app.queued_steer.as_ref().map(|steer| steer.id), Some(1));

        // The read leaves no write pending, so typing again during the same
        // turn is still guidance rather than a queued later turn.
        app.composer.set_input("and stop after that");
        assert_eq!(
            app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
            None
        );
        assert!(app.queued_input.is_none());
        let requests = commands(&app);
        assert!(matches!(
            &requests[requests.len() - 2..],
            [Request::Command(backend::Command::AdmitInput(text)),
             Request::Command(backend::Command::SteeringState)] if text == "and stop after that"
        ));
    }

    /// The read that follows an admission changes nothing, so it must not hold
    /// a later submission back. If it took the pending-write slot, typing again
    /// while it was in flight would be parked as a queued next turn instead of
    /// steering the same turn — the silent later turn the contract forbids.
    #[test]
    fn a_guidance_read_in_flight_never_defers_the_next_submission() {
        let (mut app, remote) = app();
        app.start_turn();
        app.composer.set_input("use the other branch");
        assert_eq!(
            app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
            None
        );
        // The admission's own write resolves; only its read is outstanding.
        let (admission, _) = app.requests[0].clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket: admission,
            result: Ok(Reply::Operation {
                message: "Guidance queued for the active task: use the other branch".into(),
                info: app.info.clone(),
                history: None,
                prompt: None,
            }),
        });
        assert!(
            !app.has_writes(),
            "a read in flight must not look like a pending write"
        );

        app.composer.set_input("and stop after that");
        assert_eq!(
            app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
            None
        );
        assert!(
            app.queued_input.is_none(),
            "a second guidance message must not become a queued next turn"
        );
        let requests = commands(&app);
        assert!(
            matches!(
                &requests[2..],
                [Request::Command(backend::Command::AdmitInput(text)),
                 Request::Command(backend::Command::SteeringState)] if text == "and stop after that"
            ),
            "{requests:?}"
        );
    }

    #[test]
    fn control_u_withdraws_the_queued_guidance_through_the_backend() {
        let (mut app, remote) = app();
        app.start_turn();
        // Both a queued next input and queued guidance are waiting: Ctrl+U must
        // take back the guidance the run owns and leave the queued next input
        // alone, because only the run can resolve the guidance's state.
        app.queued_input = Some("/memory search vim".into());
        app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));
        app.awaiting_user_echo = Some("use the other branch".into());
        app.composer.set_input("draft");

        app.handle_key(
            control_u(),
            &Backend::Attach(&remote),
            &CancellationToken::new(),
        );

        assert!(app.composer.input.is_empty(), "the draft is cleared");
        assert_eq!(app.queued_input.as_deref(), Some("/memory search vim"));
        assert!(matches!(
            commands(&app).as_slice(),
            [
                Request::Command(backend::Command::WithdrawSteer(3)),
                Request::Command(backend::Command::SteeringState)
            ]
        ));

        let (withdraw, _) = app.requests[0].clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket: withdraw,
            result: Ok(Reply::Operation {
                message: "Queued input withdrawn.".into(),
                info: app.info.clone(),
                history: None,
                prompt: None,
            }),
        });
        let (read, _) = app.requests[1].clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket: read,
            result: Ok(Reply::Steering(vec![steer(
                3,
                "use the other branch",
                Status::Withdrawn,
            )])),
        });

        assert!(app.queued_steer.is_none());
        assert!(
            app.awaiting_user_echo.is_none(),
            "withdrawn guidance cannot still be waiting for its echo"
        );
    }

    /// The still-live queued-next-input path: Ctrl+U clears it locally, with no
    /// backend round trip.
    #[test]
    fn control_u_still_withdraws_a_queued_next_input() {
        let (mut app, remote) = app();
        app.start_turn();
        app.queued_input = Some("/memory search vim".into());
        app.awaiting_user_echo = Some("/memory search vim".into());
        app.composer.set_input("draft");

        app.handle_key(
            control_u(),
            &Backend::Attach(&remote),
            &CancellationToken::new(),
        );

        assert!(app.queued_input.is_none());
        assert!(app.awaiting_user_echo.is_none());
        assert!(app.composer.input.is_empty());
        assert!(app.requests.is_empty());
    }

    #[test]
    fn a_turn_end_returns_undelivered_guidance_to_the_composer() {
        let (mut app, _) = app();
        app.start_turn();
        app.awaiting_user_echo = Some("use the other branch".into());
        app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

        app.end_turn();

        assert!(app.queued_steer.is_none());
        assert!(app.awaiting_user_echo.is_none());
        assert_eq!(
            app.composer.input.iter().collect::<String>(),
            "use the other branch"
        );
        assert!(matches!(
            app.requests.last().unwrap().1,
            Request::Command(backend::Command::SteeringState)
        ));

        // A draft the user typed meanwhile is never overwritten.
        app.composer.set_input("my own draft");
        app.queued_steer = Some(steer(4, "use the other branch", Status::Queued));
        app.end_turn();
        assert_eq!(
            app.composer.input.iter().collect::<String>(),
            "my own draft"
        );
    }

    /// Delivery, not the turn end, is what resolves guidance the run took: the
    /// echo is the run's own word that the text reached the model.
    #[test]
    fn a_delivered_guidance_echo_leaves_the_composer_empty_at_the_turn_end() {
        let (mut app, _) = app();
        app.start_turn();
        app.awaiting_user_echo = Some("use the other branch".into());
        app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

        app.apply_event(&WireEvent {
            event_type: USER_MESSAGE.into(),
            text: "use the other branch".into(),
            ..Default::default()
        });
        assert!(app.queued_steer.is_none());
        assert!(app.awaiting_user_echo.is_none());

        app.end_turn();
        assert!(app.composer.input.is_empty());
    }

    /// `/steer` is explicit guidance: with nothing running it reports the error
    /// the runtime gave instead of silently becoming a prompt.
    #[test]
    fn an_explicit_steer_command_never_starts_a_turn() {
        let (mut app, remote) = app();
        assert_eq!(
            app.dispatch_line(
                "/steer use the other branch",
                &Backend::Attach(&remote),
                &CancellationToken::new()
            ),
            None
        );
        assert!(matches!(
            commands(&app).as_slice(),
            [Request::Command(backend::Command::Steer(text)),
             Request::Command(backend::Command::SteeringState)] if text == "use the other branch"
        ));
        assert!(
            app.drain_actions().is_empty(),
            "an explicit steer must not dispatch a prompt"
        );
        // The runtime's reason reaches the transcript rather than a turn.
        let (ticket, _) = app.requests[0].clone();
        app.apply_completion(Completion {
            mutation: true,
            ticket,
            result: Err("there is no active task accepting guidance".into()),
        });
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.raw == "there is no active task accepting guidance"),
            "{:?}",
            app.entries
                .iter()
                .map(|e| e.raw.as_str())
                .collect::<Vec<_>>()
        );
    }
}
