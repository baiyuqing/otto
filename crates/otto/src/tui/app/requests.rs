//! Surface request queueing, guidance tracking and transitions.

use super::*;

impl App {
    pub(super) fn top_id(&self) -> Option<u64> {
        self.surfaces.top().map(|s| s.id)
    }
    pub(super) fn ticket(&mut self, surface: u64, generation: u64) -> Ticket {
        Ticket {
            workspace: self.info.workspace.clone(),
            session: self.info.session_id.clone(),
            surface,
            generation,
        }
    }
    pub(super) fn enqueue(&mut self, surface: u64, request: Request) {
        // A steering read follows the command that changed the state. Both are
        // serialized by the back end, so the read can never observe a state
        // older than the admission or withdrawal it is meant to show, and the
        // caller's display never flaps back to a stale list.
        let follows_steering = matches!(
            &request,
            Request::Command(
                backend::Command::AdmitInput(_)
                    | backend::Command::WithdrawSteer(_)
                    | backend::Command::InterruptStep
            )
        );
        let composer_feedback = matches!(
            &request,
            Request::Command(
                backend::Command::SwitchModel(_)
                    | backend::Command::SwitchModelThinking { .. }
                    | backend::Command::SetThinking { .. }
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
                    | backend::Command::SwitchModel(_)
                    | backend::Command::SwitchModelThinking { .. }
            )
        ) {
            self.session_changes.insert((surface, g));
        }
        let ticket = self.ticket(surface, g);
        self.pending.insert((surface, g));
        if composer_feedback {
            self.composer_notices.clear();
            self.composer_feedback.insert((surface, g));
        }
        if let Some(driver) = &mut self.driver {
            driver.submit(ticket, request);
        } else {
            self.requests.push((ticket, request));
        }
        if follows_steering {
            self.refresh_steering();
        }
    }

    /// Periodic reads are bounded to one outstanding steering request. Explicit
    /// post-write reads may supersede it to preserve admission ordering.
    pub(crate) fn poll_steering(&mut self) {
        if self.connected && self.session_changes.is_empty() && self.steering_generation.is_none() {
            self.refresh_steering();
        }
    }

    /// Asks the back end for the active run's guidance. The only source of
    /// [`App::queued_steer`]: an empty answer means nothing is queued any more.
    pub(crate) fn refresh_steering(&mut self) -> Ticket {
        self.enqueue(0, Request::Command(backend::Command::SteeringState));
        let generation = self.generations[&0];
        self.steering_generation = Some(generation);
        self.ticket(0, generation)
    }

    /// Paints the tracked guidance from a read. `messages` is the active run's
    /// whole transient lifecycle, oldest first, so the newest queued item is
    /// the one the user can still withdraw.
    pub(super) fn apply_steering(&mut self, messages: Vec<otto_core::agent::steering::Message>) {
        use otto_core::agent::steering::Status;

        if let Some(recalling) = self.recalling_steer.as_ref()
            && let Some(status) = messages
                .iter()
                .find(|message| message.id == recalling.id)
                .map(|message| message.status)
        {
            match status {
                Status::Withdrawn => {
                    let recalled = self.recalling_steer.take().expect("recall is present");
                    if self.composer.input.is_empty() {
                        self.composer.set_input(&recalled.text);
                    }
                    self.resolve_steering(&recalled);
                }
                Status::Delivered | Status::Discarded => {
                    let recalled = self.recalling_steer.take().expect("recall is present");
                    self.resolve_steering(&recalled);
                }
                Status::Queued => {
                    // This read is serialized after the withdrawal request. If
                    // the item is still queued, the runtime did not withdraw it.
                    self.recalling_steer = None;
                }
            }
        }
        self.delivered_steers
            .retain(|id| messages.iter().any(|message| message.id == *id));
        let previous_ids: HashSet<_> = self
            .pending_steers
            .iter()
            .map(|message| message.id)
            .collect();
        // A first-observed delivery may already be in history, or may have
        // arrived entirely between polls. Match rows by occurrence, reserving
        // rows represented by known IDs so repeated text is not collapsed.
        let mut unmatched_rows: HashMap<&str, usize> = HashMap::new();
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.kind == Some(EntryKind::User))
        {
            *unmatched_rows.entry(entry.raw.as_str()).or_default() += 1;
        }
        for message in &messages {
            if self.delivered_steers.contains(&message.id)
                && let Some(count) = unmatched_rows.get_mut(message.text.as_str())
            {
                *count = count.saturating_sub(1);
            }
        }
        let mut pending = Vec::new();
        for message in messages {
            if self.delivered_steers.contains(&message.id) {
                continue;
            }
            if message.status == Status::Delivered
                && !previous_ids.contains(&message.id)
                && let Some(count) = unmatched_rows.get_mut(message.text.as_str())
                && *count > 0
            {
                *count -= 1;
                self.delivered_steers.insert(message.id);
                continue;
            }
            if matches!(message.status, Status::Queued | Status::Delivered) {
                if self.scroll.is_some() && !previous_ids.contains(&message.id) {
                    self.new_messages = true;
                }
                pending.push(message);
            }
        }
        // A lifecycle can clear at turn end before its final notification is
        // painted. Keep already-observed deliveries until that notification.
        for previous in &self.pending_steers {
            if previous.status == Status::Delivered
                && !self.delivered_steers.contains(&previous.id)
                && !pending.iter().any(|message| message.id == previous.id)
            {
                pending.push(previous.clone());
            }
        }
        self.pending_steers = pending;
        let queued = self
            .pending_steers
            .iter()
            .rev()
            .find(|message| message.status == Status::Queued)
            .cloned();
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
    pub(super) fn resolve_steering(&mut self, steer: &otto_core::agent::steering::Message) {
        if self.awaiting_user_echo.as_deref() == Some(steer.text.as_str()) {
            self.awaiting_user_echo = None;
        }
    }
    pub(super) fn transition(&mut self, surface: u64, transition: Transition) {
        match transition {
            Transition::Stay => {}
            Transition::Close => {
                let removed = self.surfaces.remove(surface);
                self.pending.retain(|(id, _)| *id != surface);
                let Some(removed) = removed else {
                    return;
                };
                if let Some(id) = removed.model.key().strip_prefix("approval:") {
                    // The snapshot is stale until the poll below answers.
                    self.approval_snapshot.retain(|request| request.id != id);
                    self.request_approvals();
                }
                if removed.model.defers_approvals() {
                    self.release_deferred_approvals();
                }
            }
            Transition::CloseFlow => {
                let Some(flow) = self
                    .surfaces
                    .iter()
                    .find(|candidate| candidate.id == surface)
                    .map(|candidate| candidate.model.flow_key())
                else {
                    return;
                };
                let ids: Vec<_> = self
                    .surfaces
                    .iter()
                    .filter(|candidate| candidate.model.flow_key() == flow)
                    .map(|candidate| candidate.id)
                    .collect();
                for id in ids {
                    self.transition(id, Transition::Close);
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
}
