//! Turn lifecycle, refresh and surface queries.

use super::*;

impl App {
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
    pub(crate) fn enable_program_status(&mut self) {
        let reporter = crate::program_status::Reporter::default();
        reporter.report_stdout(crate::program_status::State::Idle);
        self.program_status = Some(reporter);
    }
    pub fn start_turn(&mut self) {
        self.report_program_status(crate::program_status::State::Working);
        self.start_operation("waiting for model");
    }
    pub(crate) fn report_program_status(&self, state: crate::program_status::State) {
        if let Some(reporter) = &self.program_status {
            reporter.report_stdout(state);
        }
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
    pub(crate) fn finish_turn(&mut self, completion: crate::program_status::Completion) {
        self.end_turn();
        self.report_program_status(completion.into());
    }

    /// A turn end is the last moment tracked guidance can be resolved. Guidance
    /// still tracked here never reached the model, so the text goes back into an
    /// empty composer for the user to resend. The loss itself is stated by the
    /// runtime's own `agent_steering` notice: the TUI adds no claim of its own
    /// that could contradict it.
    fn finish_steering(&mut self) {
        self.pending_steers
            .retain(|message| message.status == otto_core::agent::steering::Status::Delivered);
        self.steering_generation = None;
        let Some(steer) = self.queued_steer.take() else {
            self.recalling_steer = None;
            return;
        };
        if self.composer.input.is_empty() {
            self.composer.set_input(&steer.text);
        }
        self.recalling_steer = None;
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
            turn_elapsed: self.busy_since?.elapsed(),
        })
    }
    pub fn refresh(&mut self, backend: &Backend<'_>) -> Ticket {
        let old = (self.info.workspace.clone(), self.info.session_id.clone());
        let (mut entries, usage) = entries::entries_from_history(&backend.history());
        let info = backend.info();
        if old == (info.workspace.clone(), info.session_id.clone()) {
            for entry in &mut entries {
                if entry.tool_done
                    && entry.operation_id.is_some()
                    && let Some(previous) = self.entries.iter().find(|previous| {
                        previous.tool_done && previous.operation_id == entry.operation_id
                    })
                {
                    entry.tool_elapsed = previous.tool_elapsed;
                }
            }
        }
        let same_session = old == (info.workspace.clone(), info.session_id.clone());
        if same_session
            && self.scroll.is_some()
            && entries
                .iter()
                .filter(|entry| entry.kind == Some(EntryKind::User))
                .count()
                > self
                    .entries
                    .iter()
                    .filter(|entry| entry.kind == Some(EntryKind::User))
                    .count()
        {
            self.new_messages = true;
        }
        if same_session {
            let mut added_users: HashMap<String, usize> = HashMap::new();
            for entry in entries
                .iter()
                .filter(|entry| entry.kind == Some(EntryKind::User))
            {
                *added_users.entry(entry.raw.clone()).or_default() += 1;
            }
            for entry in self
                .entries
                .iter()
                .filter(|entry| entry.kind == Some(EntryKind::User))
            {
                if let Some(count) = added_users.get_mut(&entry.raw) {
                    *count = count.saturating_sub(1);
                }
            }
            self.pending_steers.retain(|message| {
                if let Some(count) = added_users.get_mut(&message.text)
                    && *count > 0
                {
                    *count -= 1;
                    self.delivered_steers.insert(message.id);
                    return false;
                }
                true
            });
        }
        if same_session {
            crate::tui::approval_display::retain(&mut entries, &self.entries);
        }
        self.entries = entries;
        self.usage = usage;
        self.info = info;
        self.observer
            .reset_scope(&self.info.workspace, &self.info.session_id);
        if !same_session {
            self.scroll = None;
        }
        // Same-session history may precede a foreign turn's input. Only the
        // actual added rows above reconcile projections; unmatched deliveries
        // must survive until their row or notification arrives.
        self.invalidate_reads();
        if old != (self.info.workspace.clone(), self.info.session_id.clone()) {
            self.surfaces.clear();
            // The run that owned the tracked guidance ended with the session.
            self.queued_steer = None;
            self.queued_input = None;
            self.pending_steers.clear();
            self.delivered_steers.clear();
            self.new_messages = false;
            self.awaiting_user_echo = None;
            self.optimistic_user_echo = None;
            self.recalling_steer = None;
            self.composer.reseed(prompt_history(&self.entries));
        }
        self.refresh_steering()
    }
    pub(crate) fn invalidate_reads(&mut self) {
        self.steering_generation = None;
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
    pub(crate) fn memory_review_available(&mut self) {
        self.push_system(crate::app::MEMORY_REVIEW_AVAILABLE);
        self.request_approvals();
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
}
