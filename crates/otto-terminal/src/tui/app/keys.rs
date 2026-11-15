//! Key and scroll handling.

use super::*;

impl App {
    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        backend: &Backend<'_>,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        self.selection = None;
        if super::super::input::is_ctrl_c(key) {
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
            let intent =
                super::super::input::surface_intent(key, &self.surfaces.top().unwrap().state)?;
            let activation = self.surfaces.top_mut()?.state.handle(&view, intent);
            self.surfaces.top_mut()?.state.scroll = self
                .surfaces
                .top()?
                .state
                .scroll
                .min(self.geometry.borrow().max_scroll);
            if let Some(a) = activation {
                if a == Activation::Back {
                    // Dismissing an approval must not leave a pending request
                    // hidden behind a read-only observer.
                    self.observer.active = false;
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
                    if self
                        .surfaces
                        .pop()
                        .is_some_and(|surface| surface.model.defers_approvals())
                    {
                        self.release_deferred_approvals();
                    }
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
                    self.surfaces.top_mut()?.model.failed("TUI disconnected");
                    self.surfaces.top_mut()?.state.pending = false;
                    self.surfaces.top_mut()?.state.error = Some("TUI disconnected".into());
                } else {
                    self.transition(sid, tr);
                }
            }
            return self.actions.pop();
        }
        let was_observing = self.observer.active;
        if self.observer.key(key) {
            if self.observer.active && !was_observing {
                self.observer_refreshed = None;
                self.hidden_approvals.clear();
                let snapshot = self.approval_snapshot.clone();
                self.sync_shared_approval(&snapshot);
                self.poll_observer();
            }
            return None;
        }
        if super::super::input::is_toggle_details(key) {
            self.show_details = !self.show_details;
            return None;
        }
        if super::super::input::is_help(key) && self.composer.input.is_empty() {
            return self.dispatch_line("/help", backend, cancel);
        }
        if key.code == KeyCode::End
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            self.scroll = None;
            self.new_messages = false;
            return None;
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
                KeyCode::Enter if super::super::input::is_submit(key) => {
                    self.composer.set_input(&suggestions[selected].replacement)
                }
                _ => {}
            }
        }
        if super::super::input::is_submit(key) {
            return self.submit_input(backend, cancel);
        }
        if super::super::input::is_newline(key) {
            self.composer.edit(Edit::Newline);
            return None;
        }
        if self.busy()
            || self.has_writes()
            || self.queued_input.is_some()
            || self.queued_steer.is_some()
        {
            if self.busy() && key.code == KeyCode::Esc {
                // Guidance is queued: deliver it inside this turn. A refusal
                // is shown to the user and never becomes a turn cancel.
                if self.queued_steer.is_some() {
                    self.enqueue(0, Request::Command(backend::Command::InterruptStep));
                    return None;
                }
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
                if let Some(steer) = self.queued_steer.clone() {
                    self.recalling_steer = Some(steer.clone());
                    self.enqueue(
                        0,
                        Request::Command(backend::Command::WithdrawSteer(steer.id)),
                    );
                } else if self.queued_input.is_some() {
                    if let Some(id) = self.queued_input_id.take() {
                        if let Backend::Local(controller) = backend
                            && let Err(error) =
                                controller.update_input_submission(&id, "withdrawn", None)
                        {
                            self.push_system(format!(
                                "[{id}] withdrawal status save failed: {error}; /inputs"
                            ));
                        }
                        self.push_system(format!(
                            "[{id}] withdrawn; /recover {id} restores a draft"
                        ));
                    }
                    self.queued_input = None;
                    self.awaiting_user_echo = None;
                }
                self.composer.edit(Edit::Clear);
                return None;
            }
        }
        if let Some(edit) = super::super::input::composer_edit(key) {
            self.composer.edit(edit)
        }
        self.actions.pop()
    }
    pub(crate) fn drain_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }
    pub(crate) fn scroll_wheel(&mut self, up: bool) {
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
        if self.observer.active {
            self.observer.scroll_by(if up { -1 } else { 1 });
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
            self.scroll = (next < self.max_scroll.get()).then_some(next);
        }
        if self.scroll.is_none() {
            self.new_messages = false;
        }
    }
    pub(crate) fn handle_scroll_key(&mut self, key: &KeyEvent) -> bool {
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
            self.scroll = (next < self.max_scroll.get()).then_some(next);
        }
        if self.scroll.is_none() {
            self.new_messages = false;
        }
        true
    }
}
