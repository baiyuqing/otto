//! Terminal-independent state for reusable TUI surfaces.

pub(crate) mod composer;

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct View {
    /// Draw this surface over the normal shell instead of replacing it.
    pub modal: bool,
    pub title: String,
    pub description: String,
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub collection: bool,
    pub default_row: Option<String>,
    pub document: String,
    pub fields: Vec<Field>,
    pub actions: Vec<Action>,
    pub default_action: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Row {
    pub id: String,
    pub cells: Vec<String>,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Field {
    pub id: String,
    pub label: String,
    pub value: String,
    pub multiline: bool,
    pub submit: bool,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Action {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub reason: Option<String>,
    pub shortcut: Option<char>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Intent {
    Back,
    Move(isize),
    Page(isize),
    NextFocus,
    PreviousFocus,
    Activate,
    Shortcut(char),
    Text(String),
    Backspace,
    Delete,
    Home,
    End,
    Left,
    Right,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Focus {
    Rows,
    Document,
    Field(String),
    Actions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Activation {
    Back,
    Open(String),
    Action {
        id: String,
        row: Option<String>,
        fields: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct State {
    pub selected_row: Option<String>,
    pub focus: Focus,
    pub selected_action: Option<String>,
    pub scroll: u16,
    pub page_rows: u16,
    pub drafts: BTreeMap<String, String>,
    pub pending: bool,
    pub error: Option<String>,
    field_cursors: BTreeMap<String, usize>,
    field_values: BTreeMap<String, String>,
    row_position: usize,
}

impl State {
    pub(crate) fn new(view: &View) -> Self {
        let selected_action = view
            .default_action
            .as_ref()
            .filter(|id| view.actions.iter().any(|action| &action.id == *id))
            .cloned()
            .or_else(|| {
                view.actions
                    .iter()
                    .find(|action| action.enabled)
                    .map(|a| a.id.clone())
            });
        let focus = if view.default_action.is_some() && selected_action.is_some() {
            Focus::Actions
        } else if view.collection || !view.rows.is_empty() {
            Focus::Rows
        } else if !view.document.is_empty() {
            Focus::Document
        } else if let Some(field) = view.fields.first() {
            Focus::Field(field.id.clone())
        } else {
            Focus::Actions
        };
        let mut state = Self {
            selected_row: view
                .default_row
                .as_ref()
                .filter(|id| view.rows.iter().any(|row| &row.id == *id))
                .cloned()
                .or_else(|| view.rows.first().map(|row| row.id.clone())),
            focus,
            selected_action,
            scroll: 0,
            page_rows: 1,
            drafts: BTreeMap::new(),
            pending: false,
            error: view.error.clone(),
            field_cursors: BTreeMap::new(),
            field_values: BTreeMap::new(),
            row_position: 0,
        };
        state.reconcile(view);
        state
    }

    pub(crate) fn reconcile(&mut self, view: &View) {
        let previous_row = self.selected_row.clone();
        let previous_index = self
            .selected_row
            .as_ref()
            .and_then(|id| view.rows.iter().position(|row| &row.id == id));
        if previous_index.is_none() && !view.rows.is_empty() {
            let index = self.row_position.min(view.rows.len() - 1);
            self.selected_row = if self.selected_row.is_none() {
                view.default_row
                    .as_ref()
                    .filter(|id| view.rows.iter().any(|row| &row.id == *id))
                    .cloned()
            } else {
                None
            }
            .or_else(|| Some(view.rows[index].id.clone()));
            self.row_position = view
                .rows
                .iter()
                .position(|row| Some(row.id.as_str()) == self.selected_row.as_deref())
                .unwrap_or(index);
        } else if let Some(index) = previous_index {
            self.row_position = index;
        } else {
            self.selected_row = None;
            self.row_position = 0;
        }

        if previous_row != self.selected_row {
            self.scroll = 0;
        }
        let action_exists = self
            .selected_action
            .as_ref()
            .is_some_and(|id| view.actions.iter().any(|action| &action.id == id));
        if !action_exists {
            self.selected_action = view
                .default_action
                .as_ref()
                .filter(|id| view.actions.iter().any(|action| &action.id == *id))
                .cloned()
                .or_else(|| {
                    view.actions
                        .iter()
                        .find(|a| a.enabled)
                        .map(|a| a.id.clone())
                });
        }

        let mut drafts = BTreeMap::new();
        let mut cursors = BTreeMap::new();
        let mut values = BTreeMap::new();
        for field in &view.fields {
            let previous_value = self.field_values.get(&field.id);
            let draft = match (self.drafts.get(&field.id), previous_value) {
                (Some(draft), Some(old_value)) if draft != old_value => draft.clone(),
                _ => field.value.clone(),
            };
            let cursor = self
                .field_cursors
                .get(&field.id)
                .copied()
                .unwrap_or(draft.len());
            drafts.insert(field.id.clone(), draft);
            let draft = drafts.get(&field.id).expect("inserted draft");
            // Keep a valid cursor for the selected draft, including UTF-8 boundaries.
            cursors.insert(field.id.clone(), floor_char_boundary(cursor, draft));
            values.insert(field.id.clone(), field.value.clone());
        }
        self.drafts = drafts;
        self.field_cursors = cursors;
        self.field_values = values;
        if !focus_controls(view).contains(&self.focus) {
            self.focus = first_focus(view);
        }
        self.error = view.error.clone().or(self.error.take());
    }

    pub(crate) fn field_cursor(&self, id: &str) -> usize {
        self.field_cursors
            .get(id)
            .copied()
            .unwrap_or_else(|| self.drafts.get(id).map_or(0, String::len))
    }

    pub(crate) fn handle(&mut self, view: &View, intent: Intent) -> Option<Activation> {
        if intent == Intent::Back {
            return Some(Activation::Back);
        }
        let focus = self.focus.clone();
        match intent {
            Intent::Back => unreachable!(),
            Intent::NextFocus => self.step_focus(view, 1),
            Intent::PreviousFocus => self.step_focus(view, -1),
            Intent::Move(delta) => match focus {
                Focus::Rows => self.move_row(view, delta),
                Focus::Actions => self.move_action(view, delta),
                Focus::Document => {
                    self.scroll = self.scroll.saturating_add_signed(
                        delta.clamp(i16::MIN as isize, i16::MAX as isize) as i16,
                    )
                }
                Focus::Field(id) => self.move_cursor(&id, &Intent::Move(delta)),
            },
            Intent::Page(delta) => {
                let amount = delta.signum() * self.page_rows.max(1) as isize;
                match focus {
                    Focus::Rows => self.move_row(view, amount),
                    Focus::Actions => self.move_action(view, amount),
                    Focus::Document => {
                        self.scroll = self.scroll.saturating_add_signed(
                            amount.clamp(i16::MIN as isize, i16::MAX as isize) as i16,
                        )
                    }
                    Focus::Field(id) => self.move_cursor(&id, &Intent::Move(amount)),
                }
            }
            Intent::Activate => return self.activate(view),
            Intent::Shortcut(key) if !matches!(self.focus, Focus::Field(_)) => {
                if let Some(action) = view.actions.iter().find(|action| {
                    action.enabled
                        && action
                            .shortcut
                            .is_some_and(|shortcut| shortcut.eq_ignore_ascii_case(&key))
                }) {
                    self.selected_action = Some(action.id.clone());
                    self.focus = Focus::Actions;
                    return self.activate(view);
                }
            }
            Intent::Text(text) => {
                if let Focus::Field(id) = &focus {
                    self.insert_text(view, id, &text);
                }
            }
            Intent::Backspace
            | Intent::Delete
            | Intent::Home
            | Intent::End
            | Intent::Left
            | Intent::Right => {
                if let Focus::Field(id) = &focus {
                    self.move_cursor(id, &intent);
                }
            }
            Intent::Shortcut(_) => {}
        }
        None
    }

    fn move_row(&mut self, view: &View, delta: isize) {
        if view.rows.is_empty() {
            return;
        }
        let index = self
            .selected_row
            .as_ref()
            .and_then(|id| view.rows.iter().position(|row| &row.id == id))
            .unwrap_or(0);
        self.row_position =
            (index as isize + delta).clamp(0, view.rows.len() as isize - 1) as usize;
        let next = Some(view.rows[self.row_position].id.clone());
        if self.selected_row != next {
            self.scroll = 0;
        }
        self.selected_row = next;
    }

    fn move_action(&mut self, view: &View, delta: isize) {
        let enabled: Vec<_> = view
            .actions
            .iter()
            .filter(|action| action.enabled)
            .collect();
        if enabled.is_empty() {
            return;
        }
        let index = self
            .selected_action
            .as_ref()
            .and_then(|id| enabled.iter().position(|action| &action.id == id))
            .unwrap_or(0);
        let index = (index as isize + delta).clamp(0, enabled.len() as isize - 1) as usize;
        self.selected_action = Some(enabled[index].id.clone());
    }

    fn step_focus(&mut self, view: &View, delta: isize) {
        let controls = focus_controls(view);
        if controls.is_empty() {
            return;
        }
        let index = controls
            .iter()
            .position(|focus| focus == &self.focus)
            .unwrap_or(0);
        let next = (index as isize + delta).rem_euclid(controls.len() as isize) as usize;
        self.focus = controls[next].clone();
    }

    fn activate(&mut self, view: &View) -> Option<Activation> {
        if self.pending {
            return None;
        }
        if let Focus::Field(id) = &self.focus {
            let submit = view
                .fields
                .iter()
                .any(|field| &field.id == id && field.submit);
            self.focus = Focus::Actions;
            if !submit {
                return None;
            }
        }
        match self.focus {
            Focus::Rows => self.selected_row.clone().map(Activation::Open),
            Focus::Actions if !self.pending => {
                let id = self.selected_action.as_ref()?;
                if !view
                    .actions
                    .iter()
                    .any(|action| &action.id == id && action.enabled)
                {
                    return None;
                }
                Some(Activation::Action {
                    id: id.clone(),
                    row: self.selected_row.clone(),
                    fields: self.drafts.clone(),
                })
            }
            _ => None,
        }
    }

    fn insert_text(&mut self, view: &View, id: &str, text: &str) {
        let Some(field) = view.fields.iter().find(|field| field.id == id) else {
            return;
        };
        let value = self.drafts.entry(id.to_string()).or_default();
        let cursor = self
            .field_cursors
            .entry(id.to_string())
            .or_insert(value.len());
        for ch in text.chars() {
            if (field.multiline || ch != '\n') && value.len() + ch.len_utf8() <= field.max_bytes {
                value.insert(*cursor, ch);
                *cursor += ch.len_utf8();
            }
        }
    }

    fn move_cursor(&mut self, id: &str, intent: &Intent) {
        let Some(value) = self.drafts.get(id) else {
            return;
        };
        let cursor = self
            .field_cursors
            .entry(id.to_string())
            .or_insert(value.len());
        if matches!(intent, Intent::Backspace) && *cursor == 0 {
            return;
        }
        *cursor = match intent {
            Intent::Home => 0,
            Intent::End => value.len(),
            Intent::Left => previous_boundary(value, *cursor),
            Intent::Right => next_boundary(value, *cursor),
            Intent::Move(delta) => {
                if *delta < 0 {
                    previous_boundary(value, *cursor)
                } else if *delta > 0 {
                    next_boundary(value, *cursor)
                } else {
                    *cursor
                }
            }
            Intent::Backspace => previous_boundary(value, *cursor),
            Intent::Delete => *cursor,
            _ => *cursor,
        };
        if matches!(intent, Intent::Backspace | Intent::Delete) && *cursor < value.len() {
            self.drafts.get_mut(id).unwrap().remove(*cursor);
        }
    }
}

fn first_focus(view: &View) -> Focus {
    focus_controls(view)
        .into_iter()
        .next()
        .unwrap_or(Focus::Actions)
}

fn focus_controls(view: &View) -> Vec<Focus> {
    let mut controls = Vec::new();
    if view.collection || !view.rows.is_empty() {
        controls.push(Focus::Rows);
    }
    if !view.document.is_empty() {
        controls.push(Focus::Document);
    }
    controls.extend(
        view.fields
            .iter()
            .map(|field| Focus::Field(field.id.clone())),
    );
    if !view.actions.is_empty() {
        controls.push(Focus::Actions);
    }
    controls
}

fn floor_char_boundary(index: usize, value: &str) -> usize {
    let mut index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn previous_boundary(value: &str, index: usize) -> usize {
    let index = floor_char_boundary(index, value);
    value[..index]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(value: &str, index: usize) -> usize {
    let index = floor_char_boundary(index, value);
    value[index..]
        .chars()
        .next()
        .map_or(value.len(), |ch| index + ch.len_utf8())
}

#[derive(Clone, Debug)]
pub(crate) struct Surface<T> {
    pub id: u64,
    pub model: T,
    pub state: State,
}

#[derive(Clone, Debug)]
pub(crate) struct Host<T> {
    next_id: u64,
    stack: Vec<Surface<T>>,
}

impl<T> Default for Host<T> {
    fn default() -> Self {
        Self {
            next_id: 1,
            stack: Vec::new(),
        }
    }
}

impl<T> Host<T> {
    pub(crate) fn push(&mut self, model: T, view: &View) -> u64 {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("surface id space exhausted");
        self.stack.push(Surface {
            id,
            model,
            state: State::new(view),
        });
        id
    }
    pub(crate) fn pop(&mut self) -> Option<Surface<T>> {
        self.stack.pop()
    }
    pub(crate) fn top(&self) -> Option<&Surface<T>> {
        self.stack.last()
    }
    pub(crate) fn top_mut(&mut self) -> Option<&mut Surface<T>> {
        self.stack.last_mut()
    }
    pub(crate) fn find_mut(&mut self, id: u64) -> Option<&mut Surface<T>> {
        self.stack.iter_mut().find(|surface| surface.id == id)
    }
    pub(crate) fn remove(&mut self, id: u64) -> Option<Surface<T>> {
        let index = self.stack.iter().position(|surface| surface.id == id)?;
        Some(self.stack.remove(index))
    }
    pub(crate) fn clear(&mut self) {
        self.stack.clear();
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Surface<T>> {
        self.stack.iter()
    }
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Surface<T>> {
        self.stack.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> Row {
        Row {
            id: id.into(),
            ..Row::default()
        }
    }
    fn action(id: &str) -> Action {
        Action {
            id: id.into(),
            enabled: true,
            ..Action::default()
        }
    }

    #[test]
    fn selection_tracks_id_and_missing_row_uses_neighbor() {
        let mut view = View {
            rows: vec![row("a"), row("b"), row("c")],
            ..View::default()
        };
        let mut state = State::new(&view);
        state.handle(&view, Intent::Move(1));
        view.rows = vec![row("c"), row("b"), row("a")];
        state.reconcile(&view);
        assert_eq!(state.selected_row.as_deref(), Some("b"));
        view.rows.remove(1);
        state.reconcile(&view);
        assert_eq!(state.selected_row.as_deref(), Some("a"));
    }

    #[test]
    fn initial_default_row_is_selected_but_refresh_preserves_user_selection() {
        let mut view = View {
            rows: vec![row("a"), row("b"), row("c")],
            default_row: Some("b".into()),
            ..View::default()
        };
        let mut state = State::new(&view);
        assert_eq!(state.selected_row.as_deref(), Some("b"));
        state.handle(&view, Intent::Move(1));
        assert_eq!(state.selected_row.as_deref(), Some("c"));
        view.rows.reverse();
        state.reconcile(&view);
        assert_eq!(state.selected_row.as_deref(), Some("c"));
    }

    #[test]
    fn loading_choice_keeps_collection_focus_until_rows_arrive() {
        let mut view = View {
            collection: true,
            actions: vec![Action {
                id: "apply".into(),
                label: "Apply".into(),
                enabled: false,
                ..Action::default()
            }],
            ..View::default()
        };
        let mut state = State::new(&view);
        assert_eq!(state.focus, Focus::Rows);
        assert_eq!(state.handle(&view, Intent::Activate), None);

        view.rows = vec![row("high"), row("low")];
        view.default_row = Some("high".into());
        view.actions[0].enabled = true;
        state.reconcile(&view);
        assert_eq!(state.focus, Focus::Rows);
        assert_eq!(
            state.handle(&view, Intent::Activate),
            Some(Activation::Open("high".into()))
        );
        state.handle(&view, Intent::Move(1));
        assert_eq!(state.selected_row.as_deref(), Some("low"));
    }

    #[test]
    fn loading_review_keeps_default_decision_focus_without_implicit_submit() {
        let mut view = View {
            collection: true,
            actions: vec![Action {
                id: "accept".into(),
                label: "Accept".into(),
                enabled: false,
                ..Action::default()
            }],
            default_action: Some("accept".into()),
            ..View::default()
        };
        let mut state = State::new(&view);
        assert_eq!(state.focus, Focus::Actions);
        assert_eq!(state.selected_action.as_deref(), Some("accept"));
        assert_eq!(state.handle(&view, Intent::Activate), None);

        view.rows = vec![row("candidate")];
        view.actions[0].enabled = true;
        state.reconcile(&view);
        assert_eq!(state.focus, Focus::Actions);
        assert_eq!(state.selected_row.as_deref(), Some("candidate"));
        assert_eq!(
            state.handle(&view, Intent::Activate),
            Some(Activation::Action {
                id: "accept".into(),
                row: Some("candidate".into()),
                fields: BTreeMap::new(),
            })
        );
    }

    #[test]
    fn focus_cycles_and_back_is_always_available() {
        let view = View {
            rows: vec![row("r")],
            actions: vec![action("ok")],
            ..View::default()
        };
        let mut state = State::new(&view);
        assert_eq!(state.handle(&view, Intent::Back), Some(Activation::Back));
        state.handle(&view, Intent::NextFocus);
        assert_eq!(state.focus, Focus::Actions);
    }

    #[test]
    fn drafts_survive_refresh_and_utf8_limit_is_bytes() {
        let mut view = View {
            fields: vec![Field {
                id: "name".into(),
                value: "a".into(),
                max_bytes: 3,
                ..Field::default()
            }],
            ..View::default()
        };
        let mut state = State::new(&view);
        state.handle(&view, Intent::End);
        state.handle(&view, Intent::Text("éé".into()));
        assert_eq!(state.drafts["name"], "aé");
        view.fields[0].value = "new server value".into();
        state.reconcile(&view);
        assert_eq!(state.drafts["name"], "aé");
    }

    #[test]
    fn backspace_at_start_of_utf8_field_is_a_noop() {
        let view = View {
            fields: vec![Field {
                id: "name".into(),
                value: "猫".into(),
                max_bytes: 8,
                ..Field::default()
            }],
            ..View::default()
        };
        let mut state = State::new(&view);
        state.handle(&view, Intent::Home);
        state.handle(&view, Intent::Backspace);
        assert_eq!(state.drafts["name"], "猫");
        assert_eq!(state.field_cursor("name"), 0);
    }

    #[test]
    fn reconcile_preserves_host_failure_and_actions_remain_navigable() {
        let view = View {
            actions: vec![action("accept"), action("reject")],
            default_action: Some("accept".into()),
            ..View::default()
        };
        let mut state = State::new(&view);
        state.error = Some("operation failed".into());
        state.reconcile(&view);
        assert_eq!(state.error.as_deref(), Some("operation failed"));
        state.handle(&view, Intent::Move(1));
        assert_eq!(state.selected_action.as_deref(), Some("reject"));
    }

    #[test]
    fn host_removes_and_iterates_surfaces_by_stable_id() {
        let view = View::default();
        let mut host = Host::default();
        let first = host.push("first", &view);
        let second = host.push("second", &view);
        assert_eq!(host.iter_mut().count(), 2);
        assert_eq!(
            host.remove(first).map(|surface| surface.model),
            Some("first")
        );
        assert_eq!(host.top().map(|surface| surface.id), Some(second));
    }

    #[test]
    fn pending_blocks_submission_and_default_action_only_selects() {
        let view = View {
            rows: vec![row("r")],
            actions: vec![action("accept")],
            default_action: Some("accept".into()),
            ..View::default()
        };
        let mut state = State::new(&view);
        assert_eq!(state.focus, Focus::Actions);
        assert_eq!(state.selected_action.as_deref(), Some("accept"));
        assert_eq!(
            state.handle(&view, Intent::Activate),
            Some(Activation::Action {
                id: "accept".into(),
                row: Some("r".into()),
                fields: BTreeMap::new()
            })
        );
        state.pending = true;
        assert_eq!(state.handle(&view, Intent::Activate), None);
    }

    #[test]
    fn page_moves_selection_by_geometry_and_empty_rows_do_not_open() {
        let view = View {
            rows: vec![row("a"), row("b"), row("c"), row("d")],
            ..View::default()
        };
        let mut state = State::new(&view);
        state.page_rows = 2;
        state.handle(&view, Intent::Page(1));
        assert_eq!(state.selected_row.as_deref(), Some("c"));
        let empty = View::default();
        assert_eq!(State::new(&empty).handle(&empty, Intent::Activate), None);
    }
    #[test]
    fn designated_field_submits_but_feedback_returns_to_actions_without_deciding() {
        let mut view = View {
            fields: vec![Field {
                id: "name".into(),
                value: "中文".into(),
                max_bytes: 100,
                submit: true,
                ..Default::default()
            }],
            actions: vec![Action {
                id: "save".into(),
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut state = State::new(&view);
        assert!(
            matches!(state.handle(&view,Intent::Activate),Some(Activation::Action{id,fields,..}) if id=="save"&&fields["name"]=="中文")
        );
        view.fields[0].submit = false;
        state.focus = Focus::Field("name".into());
        assert_eq!(state.handle(&view, Intent::Activate), None);
        assert_eq!(state.focus, Focus::Actions);
        state.pending = true;
        state.focus = Focus::Rows;
        state.selected_row = Some("one".into());
        view.rows = vec![Row {
            id: "one".into(),
            ..Default::default()
        }];
        assert_eq!(state.handle(&view, Intent::Activate), None);
    }
}
