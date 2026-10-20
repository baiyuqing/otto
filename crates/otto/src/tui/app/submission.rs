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
        if self.queued_input.is_some() && !current.starts_with('/') {
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
