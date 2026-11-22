//! OSC 7501 presentation state, owned by the interactive terminal UI only.
use otto_core::wire::events::{APPROVAL_DECIDED, APPROVAL_REQUESTED, WireEvent};
use otto_platform::program_status::{Completion, State};
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal, Write},
};

#[derive(Debug)]
pub(super) struct Status {
    state: State,
    approvals: BTreeSet<String>,
    pending: Vec<State>,
    enabled: bool,
    failed: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            state: State::Idle,
            approvals: BTreeSet::new(),
            pending: vec![State::Idle],
            enabled: false,
            failed: false,
        }
    }
}

impl Status {
    /// The generic loops also serve TestBackend; only the real TUI entry points enable output.
    pub(super) fn enable(&mut self) {
        self.enabled = io::stdin().is_terminal() && io::stdout().is_terminal();
    }

    pub(super) fn set(&mut self, state: State) {
        if state != State::BlockedPermission {
            self.approvals.clear();
        }
        self.transition(state);
    }

    fn transition(&mut self, state: State) {
        if self.state != state {
            self.state = state;
            self.pending.push(state);
        }
    }

    pub(super) fn observe(&mut self, event: &WireEvent) {
        if let Some(request) = &event.approval {
            if request.is_human_pending() {
                self.approvals.insert(request.id.clone());
                self.transition(State::BlockedPermission);
            } else {
                self.approvals.remove(&request.id);
                if self.approvals.is_empty() && self.state == State::BlockedPermission {
                    self.transition(State::Working);
                }
            }
            return;
        }
        match event.event_type.as_str() {
            "agent_started" => self.set(State::Working),
            APPROVAL_REQUESTED => {
                self.approvals.insert(event.approval_id.clone());
                self.transition(State::BlockedPermission);
            }
            APPROVAL_DECIDED => {
                self.approvals.remove(&event.approval_id);
                if self.approvals.is_empty() && self.state == State::BlockedPermission {
                    self.transition(State::Working);
                }
            }
            _ => {}
        }
    }

    /// Authoritative pending requests, independent of which feature panel is visible.
    pub(super) fn approvals(&mut self, ids: BTreeSet<String>, busy: bool) {
        self.approvals = ids;
        if !self.approvals.is_empty() {
            self.transition(State::BlockedPermission);
        } else if self.state == State::BlockedPermission {
            self.transition(if busy { State::Working } else { State::Idle });
        }
    }

    pub(super) fn finish(&mut self, completion: Completion) {
        self.set(completion.into());
    }

    pub(super) fn flush_stdout(&mut self) {
        if self.enabled {
            self.flush(&mut io::stdout().lock());
        } else {
            self.pending.clear();
        }
    }

    pub(super) fn flush(&mut self, writer: &mut impl Write) {
        if self.failed {
            self.pending.clear();
            return;
        }
        for state in self.pending.drain(..) {
            let value = match state {
                State::Idle => "idle",
                State::Working => "working",
                State::Done => "done",
                State::Error => "error",
                State::BlockedPermission => "blocked:kind=permission",
            };
            if write!(writer, "\x1b]7501;state={value}:app=otto\x1b\\")
                .and_then(|()| writer.flush())
                .is_err()
            {
                self.failed = true;
                break;
            }
        }
    }

    /// Transport loss or shutdown abandons live work, not a completed result.
    pub(super) fn abandon(&mut self) {
        if matches!(self.state, State::Working | State::BlockedPermission) {
            self.set(State::Idle);
        }
    }

    pub(super) fn suspend(&mut self) {
        if !self.enabled {
            return;
        }
        self.flush_stdout();
        if matches!(self.state, State::Working | State::BlockedPermission) {
            self.pending.push(State::Idle);
            self.flush_stdout();
        }
    }

    pub(super) fn resume(&mut self) {
        // Alternate-screen transitions do not reset Rex's record.
        self.pending.push(self.state);
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        self.abandon();
        self.flush_stdout();
    }
}

#[cfg(test)]
#[path = "program_status_tests.rs"]
mod tests;
