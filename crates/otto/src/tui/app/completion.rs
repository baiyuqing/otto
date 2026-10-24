//! Request completions, operation receipts and approval presentation.

use super::*;

impl App {
    pub(crate) fn apply_completion(&mut self, completion: Completion) {
        let t = completion.ticket;
        let mutation = completion.mutation;
        let composer_feedback = self.composer_feedback.remove(&(t.surface, t.generation));
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
        if t.surface == 0
            && completion.result.is_err()
            && self.steering_generation == Some(t.generation)
        {
            self.steering_generation = None;
        }
        if let Ok(Reply::Steering(messages)) = &completion.result {
            if t.surface != 0 || self.steering_generation != Some(t.generation) {
                self.pending.remove(&(t.surface, t.generation));
                return;
            }
            self.steering_generation = None;
            self.apply_steering(messages.clone());
        }
        if t.surface == 0 {
            let was_pending = self.pending.remove(&(0, t.generation));
            if !mutation
                && self.generations.get(&0) != Some(&t.generation)
                && !(was_pending && matches!(&completion.result, Ok(Reply::ApprovalSnapshot(_))))
            {
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
                        self.apply_receipt(receipt, composer_feedback)
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
                    surface.model.failed(&message);
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
            self.apply_receipt(receipt, composer_feedback);
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
    pub(super) fn apply_receipt(&mut self, receipt: OperationReceipt, composer_feedback: bool) {
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
            if composer_feedback {
                self.composer_notices = message.lines().map(str::to_owned).collect();
            } else {
                self.push_system(message);
            }
        }
        if old != (self.info.workspace.clone(), self.info.session_id.clone()) {
            self.surfaces.clear();
            self.composer_notices.clear();
            self.writes.clear();
            self.session_changes.clear();
            // The run that owned the tracked guidance ended with the session.
            self.queued_steer = None;
            self.queued_input = None;
            self.pending_steers.clear();
            self.delivered_steers.clear();
            self.new_messages = false;
            self.awaiting_user_echo = None;
            self.optimistic_user_echo = None;
            self.recalling_steer = None;
            self.invalidate_reads();
            self.composer.reseed(prompt_history(&self.entries));
            self.refresh_steering();
        }
        if let Some(draft) = draft {
            self.composer.set_input(&draft);
        }
        if let Some(prompt) = prompt {
            self.actions.push(Action::Prompt(prompt));
        }
    }
    pub(crate) fn sync_shared_approval(&mut self, requests: &[otto_core::approval::Request]) {
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
                let _ =
                    surface
                        .model
                        .receive(Reply::ApprovalSnapshot(otto_core::approval::Snapshot {
                            requests: requests.to_vec(),
                            cursor: 0,
                        }));
                surface.state.reconcile(&surface.model.view());
            }
        }
        self.approval_snapshot = requests.to_vec();
        for surface in self.surfaces.iter_mut() {
            surface.model.pending_approvals(requests);
        }
        let approval_open = self
            .surfaces
            .iter()
            .any(|surface| surface.model.key().starts_with("approval:"));
        if !approval_open
            && !self.top_defers_approvals()
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

    fn top_defers_approvals(&self) -> bool {
        self.surfaces
            .top()
            .is_some_and(|surface| surface.model.defers_approvals())
    }

    /// Presents the approvals a closed surface was holding back.
    pub(super) fn release_deferred_approvals(&mut self) {
        if self.top_defers_approvals() {
            return;
        }
        let snapshot = std::mem::take(&mut self.approval_snapshot);
        self.sync_shared_approval(&snapshot);
        for presenter in std::mem::take(&mut self.deferred_approvals) {
            self.show_approval(presenter);
        }
    }

    pub(super) fn show_approval(&mut self, presenter: features::Approval) {
        if self.top_defers_approvals() {
            self.deferred_approvals.push(presenter);
            return;
        }
        if self
            .surfaces
            .iter()
            .any(|surface| surface.model.key().starts_with("approval:"))
        {
            return;
        }
        self.transition(0, Transition::Push(Box::new(presenter)));
    }
}
