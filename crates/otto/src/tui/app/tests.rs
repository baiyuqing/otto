use super::*;
use crate::tui::interaction::{Action as SurfaceAction, Field, Row};
use otto_core::agent::steering::{Message as SteerMessage, Status};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Probe {
    key: &'static str,
    view: View,
    received: Arc<AtomicUsize>,
}
impl Presenter for Probe {
    fn key(&self) -> String {
        self.key.into()
    }
    fn view(&self) -> View {
        self.view.clone()
    }
    fn activate(&mut self, _: Activation) -> Transition {
        Transition::Stay
    }
    fn receive(&mut self, _: Reply) -> Transition {
        self.received.fetch_add(1, Ordering::SeqCst);
        Transition::Stay
    }
}
fn app() -> (App, Remote) {
    let remote = Remote::offline(Vec::new());
    let app = App::new(&Backend::Attach(&remote));
    (app, remote)
}

fn probe(key: &'static str, view: View, received: Arc<AtomicUsize>) -> Box<dyn Presenter> {
    Box::new(Probe {
        key,
        view,
        received,
    })
}
fn approval(id: &str) -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: vec![],
        schema_version: 1,
        id: id.into(),
        revision: 1,
        kind: otto_core::approval::Kind::Confirmation,
        authority: otto_core::approval::Authority::Consent,
        scope: otto_core::approval::Scope {
            workspace: "/work".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
            tool_call_id: String::new(),
            source: "test".into(),
            id: id.into(),
            version: "1".into(),
            details: "details".into(),
        },
        title: "Confirm".into(),
        reason: String::new(),
        persistent: false,
        blocking: true,
        created_at: 1,
        expires_at: None,
        state: otto_core::approval::State::Pending,
        decision: None,
        application: otto_core::approval::ApplicationState::NotRequired,
        application_error: String::new(),
        review: None,
    }
}
fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
}

fn press(app: &mut App, remote: &Remote, code: KeyCode) {
    app.handle_key(
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        &Backend::Attach(remote),
        &CancellationToken::new(),
    );
}

fn steer(id: u64, text: &str, status: otto_core::agent::steering::Status) -> SteerMessage {
    SteerMessage {
        input_provenance: None,
        id,
        text: text.into(),
        status,
        input_id: String::new(),
    }
}

fn control_u() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('u'), crossterm::event::KeyModifiers::CONTROL)
}

fn commands(app: &App) -> Vec<Request> {
    app.requests
        .iter()
        .map(|(_, request)| request.clone())
        .collect()
}

mod input_queue;
mod message_visibility;
mod message_visibility_review;
mod mutations_and_commands;
mod observer;
mod reads_and_sessions;
mod steering;
mod step_interrupt;
mod stream_continuity;
mod structured_approvals;
mod surfaces_and_approvals;
mod usage_and_tool_timing;

mod local_input_reliability;
mod local_status;
