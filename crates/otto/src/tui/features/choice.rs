use super::super::backend::{Command, Reply, Request};
use super::super::interaction::{Action, Activation, Row, View};
use super::{Presenter, Transition};

pub(crate) enum ChoiceKind {
    Resume,
    Fork,
    Archive,
    Profiles,
    Thinking { profile: Option<String> },
    Network,
    Allow(String),
}
pub(crate) struct Choice {
    kind: ChoiceKind,
    rows: Vec<Row>,
    loaded: bool,
    current: Option<String>,
    profiles: std::collections::BTreeMap<String, String>,
}

impl Choice {
    pub fn new(kind: ChoiceKind, current: &str) -> Self {
        let (rows, loaded) = match &kind {
            ChoiceKind::Thinking { .. } => (
                ["unset", "low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .map(|level| Row {
                        id: level.into(),
                        cells: vec![format!(
                            "{}{level}",
                            if level == current || (level == "unset" && current.is_empty()) {
                                "* "
                            } else {
                                ""
                            }
                        )],
                        detail: String::new(),
                    })
                    .collect(),
                true,
            ),
            ChoiceKind::Network => (
                ["deny", "allow"]
                    .into_iter()
                    .map(|mode| Row {
                        id: mode.into(),
                        cells: vec![mode.into()],
                        detail: String::new(),
                    })
                    .collect(),
                true,
            ),
            _ => (Vec::new(), false),
        };
        Self {
            kind,
            rows,
            loaded,
            current: Some(if current.is_empty() { "unset" } else { current }.into()),
            profiles: Default::default(),
        }
    }
}

impl Presenter for Choice {
    fn key(&self) -> String {
        match &self.kind {
            ChoiceKind::Resume => "resume",
            ChoiceKind::Fork => "fork",
            ChoiceKind::Archive => "archive",
            ChoiceKind::Profiles => "model",
            ChoiceKind::Thinking { .. } => "thinking",
            ChoiceKind::Network | ChoiceKind::Allow(_) => "sandbox",
        }
        .into()
    }
    fn flow_key(&self) -> String {
        match &self.kind {
            ChoiceKind::Profiles | ChoiceKind::Thinking { .. } => "model".into(),
            _ => self.key(),
        }
    }
    fn view(&self) -> View {
        let allow = matches!(self.kind, ChoiceKind::Allow(_));
        let title = match self.kind {
            ChoiceKind::Resume => "Resume session",
            ChoiceKind::Fork => "Fork session before a prompt",
            ChoiceKind::Archive => "Archive session",
            ChoiceKind::Profiles => "Switch profile",
            ChoiceKind::Thinking { .. } => "Reasoning effort",
            ChoiceKind::Network => "Sandbox network",
            ChoiceKind::Allow(_) => "Sandbox read permission",
        };
        let actions = if allow {
            vec![
                Action {
                    id: "cancel".into(),
                    label: "Cancel".into(),
                    enabled: true,
                    ..Default::default()
                },
                Action {
                    id: "apply".into(),
                    label: "Allow reading (persistent)".into(),
                    enabled: self.loaded,
                    ..Default::default()
                },
            ]
        } else {
            let mut a = vec![Action {
                id: "apply".into(),
                label: match self.kind {
                    ChoiceKind::Archive => "Archive selected session",
                    ChoiceKind::Fork => "Fork before this prompt",
                    _ => "Use selected",
                }
                .into(),
                enabled: !self.rows.is_empty(),
                ..Default::default()
            }];
            if matches!(self.kind, ChoiceKind::Thinking { .. }) {
                a.push(Action {
                    id: "save".into(),
                    label: "Save as default".into(),
                    enabled: true,
                    shortcut: Some('s'),
                    ..Default::default()
                });
            }
            a
        };
        View {
            collection: !allow,
            title: title.into(),
            rows: self.rows.clone(),
            document: match &self.kind {
                ChoiceKind::Allow(path) => format!(
                    "Permanently allow reading: {path}\nSaved to read_paths; commands remain sandboxed."
                ),
                ChoiceKind::Fork if self.loaded && self.rows.is_empty() => {
                    "This session has no prompt to fork before yet.".into()
                }
                _ => String::new(),
            },
            actions,
            default_action: allow.then(|| "cancel".into()),
            default_row: self.current.clone(),
            loading: !self.loaded,
            ..Default::default()
        }
    }
    fn activate(&mut self, activation: Activation) -> Transition {
        let (selected, save) = match activation {
            Activation::Open(id) => (Some(id), false),
            Activation::Action { id, row, .. } => {
                if id == "cancel" {
                    return Transition::Close;
                }
                (row, id == "save")
            }
            _ => return Transition::Stay,
        };
        if !matches!(self.kind, ChoiceKind::Allow(_))
            && selected
                .as_ref()
                .is_none_or(|id| !self.rows.iter().any(|row| &row.id == id))
        {
            return Transition::Stay;
        }
        let command = match &self.kind {
            ChoiceKind::Resume => Command::Resume(selected.unwrap_or_default()),
            ChoiceKind::Fork => Command::Fork(selected.unwrap_or_default()),
            ChoiceKind::Archive => Command::Archive(selected.unwrap_or_default()),
            ChoiceKind::Profiles => {
                let current = selected
                    .as_ref()
                    .and_then(|id| self.profiles.get(id))
                    .cloned()
                    .unwrap_or_default();
                return Transition::Push(Box::new(Self::new(
                    ChoiceKind::Thinking { profile: selected },
                    &current,
                )));
            }
            ChoiceKind::Thinking {
                profile: Some(profile),
            } => Command::SwitchProfileThinking {
                profile: profile.clone(),
                thinking: selected.unwrap_or_default(),
                save,
            },
            ChoiceKind::Thinking { profile: None } => Command::SetThinking {
                thinking: selected.unwrap_or_default(),
                save,
            },
            ChoiceKind::Network => Command::SandboxNetwork(selected.unwrap_or_default()),
            ChoiceKind::Allow(path) => Command::SandboxAllow(path.clone()),
        };
        Transition::Request(Request::Command(command))
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        match reply {
            Reply::Sessions(rows) => {
                self.rows = rows
                    .into_iter()
                    .map(|r| Row {
                        id: r.value,
                        cells: vec![r.label],
                        detail: String::new(),
                    })
                    .collect();
                self.loaded = true;
            }
            Reply::Profiles(profiles) => {
                self.profiles = profiles
                    .iter()
                    .map(|p| (p.name.clone(), p.thinking.clone()))
                    .collect();
                self.rows = profiles
                    .into_iter()
                    .map(|p| Row {
                        id: p.name.clone(),
                        cells: vec![p.name, format!("{}/{}", p.provider, p.model), p.thinking],
                        detail: String::new(),
                    })
                    .collect();
                self.loaded = true;
            }
            Reply::SandboxPath(path) => {
                self.kind = ChoiceKind::Allow(path);
                self.loaded = true;
            }
            Reply::Operation { .. } | Reply::Forked { .. } => return Transition::CloseFlow,
            _ => {}
        }
        Transition::Stay
    }
    fn refresh(&self) -> Option<Request> {
        match self.kind {
            ChoiceKind::Resume | ChoiceKind::Archive => Some(Request::Sessions),
            ChoiceKind::Fork => Some(Request::ForkPoints),
            ChoiceKind::Profiles => Some(Request::Profiles),
            ChoiceKind::Allow(ref path) => Some(Request::SandboxPath(path.clone())),
            _ => None,
        }
    }
}
