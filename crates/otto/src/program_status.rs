//! Terminal-native program lifecycle reporting through OSC 7501.
//!
//! The protocol is presentation-neutral: Otto reports one root record and the
//! terminal decides whether to show a tab indicator, notification, or inbox
//! item. Reports are best-effort and contain only fixed, non-sensitive text.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use otto_core::agent::Event;
use otto_core::approval::State as ApprovalState;
use otto_core::wire::events::{APPROVAL_DECIDED, APPROVAL_REQUESTED, WireEvent};

const APP: &str = "otto";

/// Root-record states Otto exposes to a supporting terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    BlockedPermission,
    Done,
    Error,
}

impl State {
    pub fn finished(success: bool, canceled: bool) -> Self {
        if canceled {
            Self::Idle
        } else if success {
            Self::Done
        } else {
            Self::Error
        }
    }

    fn fields(self) -> (&'static str, Option<&'static str>, &'static str) {
        match self {
            Self::Idle => ("idle", None, "Waiting for instructions"),
            Self::Working => ("working", None, "Working"),
            Self::BlockedPermission => ("blocked", Some("permission"), "Waiting for approval"),
            Self::Done => ("done", None, "Finished"),
            Self::Error => ("error", None, "Failed"),
        }
    }
}

/// Deduplicates root-record reports while the frontend owns the writer.
///
/// A failed write disables later reports so terminal status can never make an
/// Otto operation fail.
#[derive(Debug, Default)]
pub struct Reporter {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    last: Option<State>,
    pending_approvals: BTreeSet<String>,
    failed: bool,
}

impl Reporter {
    /// Reports a state once. I/O is intentionally best-effort.
    pub fn report(&self, writer: &mut dyn Write, state: State) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state != State::BlockedPermission {
            inner.pending_approvals.clear();
        }
        write_state(&mut inner, writer, state);
    }

    pub fn report_stdout(&self, state: State) {
        self.report(&mut io::stdout(), state);
    }

    /// Projects lifecycle-bearing local events onto protocol states.
    pub fn observe(&self, writer: &mut dyn Write, event: &Event) {
        match event {
            Event::AgentStarted => self.report(writer, State::Working),
            Event::ApprovalChanged { request } => {
                let mut inner = self
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if request.state == ApprovalState::Pending && request.is_human_pending() {
                    inner.pending_approvals.insert(request.id.clone());
                    write_state(&mut inner, writer, State::BlockedPermission);
                } else {
                    inner.pending_approvals.remove(&request.id);
                    if inner.pending_approvals.is_empty()
                        && inner.last == Some(State::BlockedPermission)
                    {
                        write_state(&mut inner, writer, State::Working);
                    }
                }
            }
            _ => {}
        }
    }

    /// Projects attached-server frames onto protocol states.
    pub fn observe_wire(&self, writer: &mut dyn Write, event: &WireEvent) {
        match event.event_type.as_str() {
            "agent_started" => self.report(writer, State::Working),
            APPROVAL_REQUESTED => {
                let mut inner = self
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner.pending_approvals.insert(event.approval_id.clone());
                write_state(&mut inner, writer, State::BlockedPermission);
            }
            APPROVAL_DECIDED => {
                let mut inner = self
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner.pending_approvals.remove(&event.approval_id);
                if inner.pending_approvals.is_empty()
                    && inner.last == Some(State::BlockedPermission)
                {
                    write_state(&mut inner, writer, State::Working);
                }
            }
            _ => {}
        }
    }

    pub fn observe_wire_stdout(&self, event: &WireEvent) {
        self.observe_wire(&mut io::stdout(), event);
    }
}

fn write_state(inner: &mut Inner, writer: &mut dyn Write, state: State) {
    if inner.failed || inner.last == Some(state) {
        return;
    }
    if writer
        .write_all(&encode(state))
        .and_then(|()| writer.flush())
        .is_err()
    {
        inner.failed = true;
        return;
    }
    inner.last = Some(state);
}

fn encode(state: State) -> Vec<u8> {
    let (state, kind, message) = state.fields();
    let message = BASE64.encode(message);
    let kind = kind.map(|kind| format!(":kind={kind}")).unwrap_or_default();
    format!("\x1b]7501;state={state}{kind}:app={APP}:msg={message}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_takes_priority_in_final_state_classification() {
        assert_eq!(State::finished(true, false), State::Done);
        assert_eq!(State::finished(false, false), State::Error);
        assert_eq!(State::finished(true, true), State::Idle);
        assert_eq!(State::finished(false, true), State::Idle);
    }

    #[test]
    fn encodes_a_blocked_root_record_with_fixed_safe_text() {
        assert_eq!(
            String::from_utf8(encode(State::BlockedPermission)).expect("utf8"),
            "\u{1b}]7501;state=blocked:kind=permission:app=otto:msg=V2FpdGluZyBmb3IgYXBwcm92YWw=\u{1b}\\"
        );
    }

    #[test]
    fn repeated_states_are_not_written_twice() {
        let reporter = Reporter::default();
        let mut output = Vec::new();

        reporter.report(&mut output, State::Working);
        reporter.report(&mut output, State::Working);
        reporter.report(&mut output, State::Done);

        let output = String::from_utf8(output).expect("utf8");
        assert_eq!(output.matches("\u{1b}]7501;").count(), 2);
        assert!(output.contains("state=working"));
        assert!(output.contains("state=done"));
    }

    #[test]
    fn attached_events_follow_the_turn_and_approval_lifecycle() {
        let reporter = Reporter::default();
        let mut output = Vec::new();
        reporter.report(&mut output, State::Idle);
        reporter.observe_wire(
            &mut output,
            &WireEvent {
                event_type: "agent_started".into(),
                ..WireEvent::default()
            },
        );
        reporter.observe_wire(
            &mut output,
            &WireEvent {
                event_type: APPROVAL_REQUESTED.into(),
                ..WireEvent::default()
            },
        );
        reporter.observe_wire(
            &mut output,
            &WireEvent {
                event_type: APPROVAL_DECIDED.into(),
                ..WireEvent::default()
            },
        );
        reporter.report(&mut output, State::Done);

        let output = String::from_utf8(output).expect("utf8");
        let states: Vec<_> = output
            .split("\u{1b}]7501;state=")
            .skip(1)
            .map(|report| report.split([':', ';']).next().expect("state"))
            .collect();
        assert_eq!(states, ["idle", "working", "blocked", "working", "done"]);
    }

    #[test]
    fn one_decision_does_not_clear_another_pending_approval() {
        let reporter = Reporter::default();
        let mut output = Vec::new();
        for id in ["approval-1", "approval-2"] {
            reporter.observe_wire(
                &mut output,
                &WireEvent {
                    event_type: APPROVAL_REQUESTED.into(),
                    approval_id: id.into(),
                    ..WireEvent::default()
                },
            );
        }
        reporter.observe_wire(
            &mut output,
            &WireEvent {
                event_type: APPROVAL_DECIDED.into(),
                approval_id: "approval-1".into(),
                ..WireEvent::default()
            },
        );
        let working = encode(State::Working);
        assert_eq!(
            output
                .windows(working.len())
                .filter(|window| *window == working.as_slice())
                .count(),
            0
        );

        reporter.observe_wire(
            &mut output,
            &WireEvent {
                event_type: APPROVAL_DECIDED.into(),
                approval_id: "approval-2".into(),
                ..WireEvent::default()
            },
        );
        assert_eq!(
            output
                .windows(working.len())
                .filter(|window| *window == working.as_slice())
                .count(),
            1
        );
    }

    #[test]
    fn every_state_stays_below_the_protocol_limit() {
        for state in [
            State::Idle,
            State::Working,
            State::BlockedPermission,
            State::Done,
            State::Error,
        ] {
            let report = encode(state);
            assert!(report.len() < 4096);
            assert!(report.ends_with(b"\x1b\\"));
        }
    }
}
