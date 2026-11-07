//! Session details and clipboard actions stay on the terminal client, including attach.
use super::{Presenter, Transition};
use crate::app::Info;
use crate::tui::backend::Reply;
use crate::tui::interaction::{Action, Activation, View};

pub(super) struct SessionDetails {
    session_id: String,
    details: String,
    feedback: Option<String>,
    copy: fn(&str) -> std::io::Result<()>,
}

impl SessionDetails {
    pub(super) fn new(info: &Info) -> Self {
        Self {
            session_id: info.session_id.clone(),
            details: format!(
                "{}\nID: {}\nWorkspace: {}\nModel: {} (type {})\nReasoning: {}{}",
                info.session_name,
                info.session_id,
                info.workspace,
                info.model_selector,
                info.provider_type,
                info.thinking,
                info.forked_from
                    .as_ref()
                    .map(|origin| format!("\nForked from: {}", origin.session_id))
                    .unwrap_or_default()
            ),
            feedback: None,
            copy: crate::tui::selection::copy,
        }
    }
}

impl Presenter for SessionDetails {
    fn key(&self) -> String {
        "session".into()
    }

    fn view(&self) -> View {
        View {
            title: "Session".into(),
            document: self.details.clone(),
            actions: [
                ("session-copy-id", "Copy Session ID"),
                ("session-copy-details", "Copy all displayed details"),
            ]
            .into_iter()
            .map(|(id, label)| Action {
                id: id.into(),
                label: label.into(),
                enabled: true,
                ..Default::default()
            })
            .collect(),
            default_action: Some("session-copy-id".into()),
            vertical_actions: true,
            notice: self.feedback.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, activation: Activation) -> Transition {
        match activation {
            Activation::Back => return Transition::Close,
            Activation::Action { id, .. } => {
                let (payload, label) = match id.as_str() {
                    "session-copy-id" => (&self.session_id, "Session ID"),
                    "session-copy-details" => (&self.details, "All displayed details"),
                    _ => return Transition::Stay,
                };
                self.feedback = Some(match (self.copy)(payload) {
                    Ok(()) => format!("{label} copied."),
                    Err(error) => format!(
                        "Copy failed: {error}. Check your terminal host's clipboard helper and retry."
                    ),
                });
            }
            _ => {}
        }
        Transition::Stay
    }

    fn receive(&mut self, _: Reply) -> Transition {
        Transition::Stay
    }
}

#[cfg(test)]
#[path = "session_details_tests.rs"]
mod tests;
