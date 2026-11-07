//! Terminal input bindings. Features only receive semantic activations.

use super::interaction::composer::Edit;
use super::interaction::{Focus, Intent, State, View};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn is_toggle_details(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL)
}
pub(crate) fn is_help(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('?') && key.modifiers.is_empty()
}

pub(crate) fn is_submit(key: KeyEvent) -> bool {
    key.code == KeyCode::Enter && !is_newline(key)
}

pub(crate) fn is_newline(key: KeyEvent) -> bool {
    key.code == KeyCode::Enter
        && key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
}

pub(crate) fn surface_intent(key: KeyEvent, state: &State) -> Option<Intent> {
    let field = matches!(state.focus, Focus::Field(_));
    Some(match key.code {
        KeyCode::Esc => Intent::Back,
        KeyCode::Tab => Intent::NextFocus,
        KeyCode::BackTab => Intent::PreviousFocus,
        KeyCode::Enter if is_newline(key) && field => Intent::Text("\n".into()),
        KeyCode::Enter => Intent::Activate,
        KeyCode::Up => Intent::Move(-1),
        KeyCode::Down => Intent::Move(1),
        KeyCode::Left if !field => Intent::MoveAction(-1),
        KeyCode::Right if !field => Intent::MoveAction(1),
        KeyCode::Left => Intent::Left,
        KeyCode::Right => Intent::Right,
        KeyCode::Home => Intent::Home,
        KeyCode::End => Intent::End,
        KeyCode::PageUp => Intent::Page(-1),
        KeyCode::PageDown => Intent::Page(1),
        KeyCode::Backspace => Intent::Backspace,
        KeyCode::Delete => Intent::Delete,
        KeyCode::Char('a') if field && key.modifiers.contains(KeyModifiers::CONTROL) => {
            Intent::Home
        }
        KeyCode::Char('e') if field && key.modifiers.contains(KeyModifiers::CONTROL) => Intent::End,
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if field {
                Intent::Text(ch.to_string())
            } else {
                Intent::Shortcut(ch)
            }
        }
        _ => return None,
    })
}

pub(crate) fn composer_edit(key: KeyEvent) -> Option<Edit> {
    Some(match key.code {
        KeyCode::Enter if is_newline(key) => Edit::Newline,
        KeyCode::Backspace => Edit::Backspace,
        KeyCode::Delete => Edit::Delete,
        KeyCode::Left => Edit::Left,
        KeyCode::Right => Edit::Right,
        KeyCode::Home => Edit::Home,
        KeyCode::End => Edit::End,
        KeyCode::Up => Edit::PreviousHistory,
        KeyCode::Down => Edit::NextHistory,
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => Edit::LineStart,
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => Edit::LineEnd,
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => Edit::Clear,
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Edit::Insert(ch.to_string())
        }
        _ => return None,
    })
}

pub(crate) fn hints(view: &View, state: &State) -> String {
    if view
        .actions
        .iter()
        .any(|action| action.id == "session-copy-id")
    {
        return "↑/↓ select · Enter copy · Esc close".into();
    }
    let mut hints = vec!["Tab focus", "↑/↓ navigate", "Enter select", "Esc back"]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if view.actions.iter().filter(|action| action.enabled).count() > 1 {
        hints.insert(2, "←/→ actions".into());
    }
    if !matches!(state.focus, Focus::Field(_)) {
        hints.extend(
            view.actions
                .iter()
                .filter(|a| a.enabled)
                .filter_map(|a| a.shortcut.map(|key| format!("{key} {}", a.label))),
        );
    }
    hints.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn horizontal_arrows_select_actions_without_reusing_vertical_navigation() {
        let view = View {
            actions: vec![
                super::super::interaction::Action {
                    id: "accept".into(),
                    enabled: true,
                    ..Default::default()
                },
                super::super::interaction::Action {
                    id: "reject".into(),
                    enabled: true,
                    ..Default::default()
                },
            ],
            default_action: Some("accept".into()),
            ..View::default()
        };
        let state = State::new(&view);
        assert_eq!(
            surface_intent(KeyEvent::from(KeyCode::Left), &state),
            Some(Intent::MoveAction(-1))
        );
        assert_eq!(
            surface_intent(KeyEvent::from(KeyCode::Right), &state),
            Some(Intent::MoveAction(1))
        );
        assert_eq!(
            surface_intent(KeyEvent::from(KeyCode::Down), &state),
            Some(Intent::Move(1))
        );
    }

    #[test]
    fn chinese_text_and_shortcuts_follow_the_focused_control() {
        let view = View::default();
        let mut state = State::new(&view);
        state.focus = Focus::Field("feedback".into());
        assert_eq!(
            surface_intent(KeyEvent::from(KeyCode::Char('是')), &state),
            Some(Intent::Text("是".into()))
        );
        assert_eq!(
            surface_intent(KeyEvent::from(KeyCode::Char('y')), &state),
            Some(Intent::Text("y".into()))
        );
        state.focus = Focus::Actions;
        assert_eq!(
            surface_intent(KeyEvent::from(KeyCode::Char('y')), &state),
            Some(Intent::Shortcut('y'))
        );
    }
}
