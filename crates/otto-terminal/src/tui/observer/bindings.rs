//! Observer-only binding grammar; configuration is resolved before raw mode.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use otto_core::config::ui::ObserverBindings;

use crate::tui::layout::escape_single_line_text;

#[derive(Clone, Debug)]
pub(crate) struct Bindings {
    toggle: Option<KeyEvent>,
    open: Option<KeyEvent>,
    back: Option<KeyEvent>,
    pub label: String,
    pub back_label: String,
    pub entry_label: String,
}

impl Default for Bindings {
    fn default() -> Self {
        Self::parse(&ObserverBindings::default()).expect("default observer keys")
    }
}

impl Bindings {
    pub fn parse(bindings: &ObserverBindings) -> Result<Self, String> {
        let toggle = parse_key(&bindings.toggle)?;
        let open = parse_key(&bindings.open)?;
        let back = parse_key(&bindings.back)?;
        if toggle.is_none() && (open.is_none() || back.is_none()) {
            return Err("ui.observer needs a toggle, or both open and back bindings".into());
        }
        if [toggle, open, back]
            .into_iter()
            .flatten()
            .any(|key| key.code == KeyCode::End && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Err(
                "ui.observer cannot override Ctrl+End follow-output; use F6 or another binding"
                    .into(),
            );
        }
        for key in [toggle, open].into_iter().flatten() {
            if matches!(
                key.code,
                KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc
            ) || (matches!(key.code, KeyCode::Char(_))
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code,
                    KeyCode::Char(character) if "aceouvsjm".contains(character.to_ascii_lowercase())))
                || (key.modifiers.is_empty() && !matches!(key.code, KeyCode::F(_)))
            {
                return Err("ui.observer open/toggle must not override typing, submission, paste or composer controls; use F6 or modified arrows/letters".into());
            }
        }
        if back.is_some_and(|key| {
            matches!(
                key.code,
                KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            )
        }) {
            return Err("ui.observer back must not override pane navigation or scrolling".into());
        }
        let keys = [toggle, open, back];
        for (index, key) in keys.iter().enumerate() {
            if key.is_some() && keys[..index].contains(key) {
                return Err("ui.observer bindings must be distinct".into());
            }
        }
        let labels = [
            (&bindings.toggle, "toggle"),
            (&bindings.open, "open"),
            (&bindings.back, "main"),
        ]
        .into_iter()
        .filter(|(key, _)| !key.trim().is_empty())
        .map(|(key, action)| format!("{}: {action}", escape_single_line_text(key.trim())))
        .collect::<Vec<_>>()
        .join(" · ");
        let entry_label = escape_single_line_text(if toggle.is_some() {
            bindings.toggle.trim()
        } else {
            bindings.open.trim()
        });
        let back_label = escape_single_line_text(if toggle.is_some() {
            bindings.toggle.trim()
        } else {
            bindings.back.trim()
        });
        Ok(Self {
            toggle,
            open,
            back,
            label: labels,
            back_label,
            entry_label,
        })
    }

    pub fn toggle(&self, key: KeyEvent) -> bool {
        matches_key(self.toggle, key)
    }
    pub fn open(&self, key: KeyEvent) -> bool {
        matches_key(self.open, key)
    }
    pub fn back(&self, key: KeyEvent) -> bool {
        matches_key(self.back, key)
    }
}

fn matches_key(expected: Option<KeyEvent>, actual: KeyEvent) -> bool {
    expected.is_some_and(|expected| {
        expected.code == actual.code && expected.modifiers == actual.modifiers
    })
}

fn parse_key(value: &str) -> Result<Option<KeyEvent>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let error =
        || format!("Invalid ui.observer key '{value}'; use e.g. F6, Ctrl+Left, Alt+o, or Esc");
    let mut parts = value.split('+').peekable();
    let mut modifiers = KeyModifiers::NONE;
    let mut code = None;
    while let Some(part) = parts.next() {
        let lower = part.trim().to_ascii_lowercase();
        if parts.peek().is_some() {
            let modifier = match lower.as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => return Err(error()),
            };
            if modifiers.contains(modifier) {
                return Err(error());
            }
            modifiers.insert(modifier);
        } else {
            code = Some(match lower.as_str() {
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pgup" | "pageup" => KeyCode::PageUp,
                "pgdn" | "pagedown" => KeyCode::PageDown,
                "esc" | "escape" => KeyCode::Esc,
                "tab" => KeyCode::Tab,
                "enter" => KeyCode::Enter,
                "space" => KeyCode::Char(' '),
                _ if lower.starts_with('f')
                    && lower[1..]
                        .parse::<u8>()
                        .is_ok_and(|number| (1..=12).contains(&number)) =>
                {
                    KeyCode::F(lower[1..].parse().map_err(|_| error())?)
                }
                _ if lower.chars().count() == 1 => {
                    let character = lower.chars().next().ok_or_else(error)?;
                    if character.is_control() {
                        return Err(error());
                    }
                    KeyCode::Char(if modifiers.contains(KeyModifiers::SHIFT) {
                        character.to_ascii_uppercase()
                    } else {
                        character
                    })
                }
                _ => return Err(error()),
            });
        }
    }
    let mut code = code.ok_or_else(error)?;
    if code == KeyCode::Tab && modifiers.contains(KeyModifiers::SHIFT) {
        code = KeyCode::BackTab;
    }
    // Common legacy terminal encodings collapse these chords into other keys.
    // Reject them rather than promising a distinct event across terminal protocols.
    if modifiers.contains(KeyModifiers::CONTROL)
        && matches!(code, KeyCode::Char(character)
            if "[imjh? ".contains(character.to_ascii_lowercase()))
    {
        return Err(format!(
            "Invalid ui.observer key '{value}': ambiguous terminal Ctrl alias; use F6 or modified arrows instead (Esc is allowed for back)"
        ));
    }
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'C')) {
        return Err("ui.observer cannot override Ctrl+C cancellation".into());
    }
    #[cfg(unix)]
    if super::super::terminal_input::is_suspend_key(KeyEvent::new(code, modifiers)) {
        return Err("ui.observer cannot override Ctrl+Z job suspension".into());
    }
    Ok(Some(KeyEvent::new(code, modifiers)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_do_not_capture_control_arrows() {
        let bindings = Bindings::default();
        assert!(bindings.toggle(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)));
        assert!(!bindings.open(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)));
        assert!(!bindings.back(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)));
    }

    #[test]
    fn custom_arrows_and_empty_disable_are_supported() {
        let bindings = Bindings::parse(&ObserverBindings {
            toggle: String::new(),
            open: "Ctrl+Left".into(),
            back: "Ctrl+Right".into(),
        })
        .unwrap();
        assert!(bindings.open(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)));
        assert!(bindings.back(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)));
        assert!(!bindings.toggle(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)));
        assert!(bindings.label.contains("Ctrl+Right: main"));
        assert_eq!(
            bindings.entry_label, "Ctrl+Left",
            "entry hint must show the open key"
        );
        assert_eq!(
            bindings.back_label, "Ctrl+Right",
            "return hint must show the back key"
        );
    }

    #[test]
    fn rejects_terminal_control_aliases_before_conflict_checks() {
        for alias in [
            "Ctrl+[",
            "Ctrl+I",
            "Ctrl+M",
            "Ctrl+J",
            "Ctrl+H",
            "Ctrl+?",
            "Ctrl+Space",
            "cOnTrOl+i",
            "CONTROL+SHIFT+I",
            "Alt+Ctrl+[",
        ] {
            for action in ["toggle", "open", "back"] {
                let mut config = ObserverBindings::default();
                match action {
                    "toggle" => config.toggle = alias.into(),
                    "open" => config.open = alias.into(),
                    "back" => config.back = alias.into(),
                    _ => unreachable!(),
                }
                let error = Bindings::parse(&config).unwrap_err();
                assert!(
                    error.contains("ambiguous terminal Ctrl alias"),
                    "{action}: {alias}: {error}"
                );
                assert!(error.contains("F6"));
            }
        }
    }

    #[test]
    fn reserves_control_end_for_every_observer_action() {
        for alias in ["Ctrl+End", "control+end", "cOnTrOl+EnD", "Shift+Ctrl+End"] {
            for action in ["toggle", "open", "back"] {
                let mut config = ObserverBindings::default();
                match action {
                    "toggle" => config.toggle = alias.into(),
                    "open" => config.open = alias.into(),
                    "back" => config.back = alias.into(),
                    _ => unreachable!(),
                }
                let error = Bindings::parse(&config).unwrap_err();
                assert!(
                    error.contains("Ctrl+End follow-output"),
                    "{action}: {alias}: {error}"
                );
            }
        }
    }

    #[test]
    fn escape_back_and_default_f6_remain_legal() {
        for alias in ["Esc", "escape", "ESCAPE"] {
            let bindings = Bindings::parse(&ObserverBindings {
                back: alias.into(),
                ..Default::default()
            })
            .unwrap();
            assert!(bindings.back(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            assert!(bindings.toggle(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)));
        }
    }

    #[test]
    fn binding_labels_escape_controls_and_literal_control_keys_are_rejected() {
        let bindings = Bindings::parse(&ObserverBindings {
            toggle: "Ctrl\n+Left".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(bindings.label.contains("Ctrl\\x0a+Left"));
        assert_eq!(bindings.back_label, "Ctrl\\x0a+Left");
        assert!(
            Bindings::parse(&ObserverBindings {
                back: "\x1b".into(),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn rejects_conflicts_bad_keys_and_no_escape_route() {
        for config in [
            ObserverBindings {
                toggle: "Ctrl+C".into(),
                ..Default::default()
            },
            ObserverBindings {
                toggle: "F99".into(),
                ..Default::default()
            },
            ObserverBindings {
                toggle: "F6".into(),
                open: "f6".into(),
                ..Default::default()
            },
            ObserverBindings {
                toggle: String::new(),
                open: "F7".into(),
                back: String::new(),
            },
        ] {
            assert!(Bindings::parse(&config).is_err());
        }
    }
}
