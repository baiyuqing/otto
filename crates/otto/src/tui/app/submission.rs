//! Input submission, skill completions and suggestions.

use super::*;

impl App {
    pub fn dispatch_line(
        &mut self,
        line: &str,
        _backend: &Backend<'_>,
        _cancel: &CancellationToken,
    ) -> Option<Action> {
        self.composer_notices.clear();
        if line.starts_with('/') {
            if !self.connected {
                self.push_system("TUI disconnected");
                return None;
            }
            if line.trim() == "/help" {
                if let Ok(Transition::Push(help)) =
                    self.catalog.open(line, &self.info, self.attached)
                {
                    let mut text = help.view().document;
                    text.push_str(&format!(
                        concat!(
                            "\n\nSub-agent observer (read-only)\n{}\n",
                            "Tab/Shift+Tab: focus panes/pages · Up/Down/PgUp/PgDn: scroll\n",
                            "Home: oldest output · End: follow output\n",
                            "Draft and main scroll are preserved; approvals and Ctrl+C retain normal controls.\n",
                            "Configure [ui.observer] toggle/open/back in your user config; restart to apply."
                        ),
                        self.observer.bindings.label
                    ));
                    self.transition(
                        0,
                        Transition::Push(Box::new(features::Document {
                            key: "help".into(),
                            title: "Commands".into(),
                            text,
                        })),
                    );
                }
                return self.actions.pop();
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
                self.optimistic_user_echo = Some(line.into());
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
        if self.has_writes() || (self.queued_input_id.is_some() && self.busy()) {
            return None;
        }
        if self
            .queued_input
            .as_ref()
            .is_some_and(|line| !line.starts_with('/'))
            && let Backend::Local(controller) = backend
            && controller.pending_approval().is_some()
        {
            return None;
        }
        let line = self.queued_input.take()?;
        let input_id = self.queued_input_id.take();
        if let Some(input_id) = input_id {
            return Some(Action::LocalPrompt {
                text: line,
                input_id,
                delivery: otto_core::model::InputDelivery::Queued,
            });
        }
        let action = self.dispatch_line(&line, backend, cancel);
        if !self.attached && matches!(action, Some(Action::Prompt(_))) {
            self.next_input_delivery = Some(otto_core::model::InputDelivery::Queued);
        }
        action
    }
    pub(crate) fn submit_input(
        &mut self,
        backend: &Backend<'_>,
        cancel: &CancellationToken,
    ) -> Option<Action> {
        let current = self.composer.input.iter().collect::<String>();
        if let Backend::Local(controller) = backend {
            if current.trim() == "/inputs" {
                match controller.input_submissions() {
                    Ok(records) => {
                        self.push_system(format!("Inputs for session {} in {} (source / delivery / status; recover with /recover <id>):", self.info.session_id, self.info.workspace));
                        for record in records {
                            let status = if controller.has_input(&record.id) {
                                "delivered"
                            } else {
                                &record.state
                            };
                            self.push_system(format!(
                                "[{}] {:?} / {:?} / {}",
                                record.id,
                                record.provenance.source,
                                record.provenance.delivery,
                                status
                            ));
                        }
                    }
                    Err(error) => self.push_system(error),
                }
                self.composer.take();
                return None;
            }
            if let Some(id) = current.trim().strip_prefix("/recover ") {
                match controller.input_submissions().and_then(|records| {
                    records
                        .into_iter()
                        .find(|record| record.id == id)
                        .ok_or_else(|| "Input ID not found in this session; use /inputs".into())
                }) {
                    Ok(record) => {
                        self.composer.set_input(&record.text);
                        self.push_system(format!("[{}] recovered as a draft only; check /inputs and effects before Enter (a new ID). Nothing was replayed.", record.id));
                    }
                    Err(error) => self.push_system(error),
                }
                return None;
            }
        }
        if self.queued_input.is_some() && !current.starts_with('/') {
            if let Some(id) = &self.queued_input_id {
                self.push_system(format!("[{id}] still queued; new text retained in the editor. Ctrl+U withdraws the queued input; /inputs shows status."));
            }
            return None;
        }
        let mut local_id = None;
        if !current.trim().is_empty()
            && !current.starts_with('/')
            && let Backend::Local(controller) = backend
        {
            let delivery = if self.has_writes() {
                otto_core::model::InputDelivery::Queued
            } else if self.busy() {
                otto_core::model::InputDelivery::Steering
            } else {
                otto_core::model::InputDelivery::Direct
            };
            match controller.prepare_input(&current, delivery) {
                Ok(record) => {
                    self.push_system(format!("[{}] Tui / {:?} / pending", record.id, delivery));
                    local_id = Some(record.id);
                }
                Err(error) => {
                    self.push_system(format!("Input NOT sent; original draft retained. {error}. Check session storage/permissions, then retry Enter."));
                    return None;
                }
            }
        }
        let line = self.composer.take()?;
        if !line.starts_with('/') {
            self.scroll = None;
            self.new_messages = false;
            self.awaiting_user_echo = Some(line.clone());
        }
        if self.has_writes() && !line.starts_with('/') {
            self.queued_input = Some(line);
            self.queued_input_id = local_id;
            return None;
        }
        if self.busy() && !line.starts_with('/') {
            // Natural language submitted during an active turn is guidance for
            // that exact turn, never an implicit later queued prompt. The
            // backend resolves the admission race: only an idle result starts
            // a new prompt explicitly.
            let command = match local_id {
                Some(input_id) => backend::Command::AdmitLocalInput {
                    text: line,
                    input_id,
                },
                None => backend::Command::AdmitInput(line),
            };
            self.enqueue(0, Request::Command(command));
            return None;
        }
        if let Some(input_id) = local_id {
            return Some(Action::LocalPrompt {
                text: line,
                input_id,
                delivery: otto_core::model::InputDelivery::Direct,
            });
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
        } else if !self.observer.active {
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

    pub(crate) fn suggestions(&self) -> Vec<super::super::commands::Completion> {
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
            .map(|entry| super::super::commands::Completion {
                replacement: entry.name.into(),
                description: entry.description.into(),
            })
            .collect()
    }
}
