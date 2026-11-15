//! Turn-bound steering and model-originated follow-up admission.
//!
//! [`TaskControl`] holds the follow-up queue and steering state of one runner.
//! `tools` holds the model-callable `task_queue`, `task_steer` and `task_reply`
//! tools, which runtime assembly registers; `crate::app::task_control` builds the
//! control dialogue on top of both.

pub(crate) mod tools;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use otto_core::agent::inbox::{MailboxError, Notification, NotificationKind};
use otto_core::agent::steering::{Steering, SubmitError};
use otto_core::mailbox::Mailbox;

const MAX_FOLLOWUPS: usize = 16;
const MAX_TEXT_BYTES: usize = 32_000;

#[derive(Default)]
struct State {
    generation: u64,
    active: Option<ActiveRun>,
}

struct ActiveRun {
    session_id: String,
    original_task: String,
    current_task: String,
    /// Identifies this logical task's follow-ups in the journal.
    scope: String,
    followups: VecDeque<Followup>,
    accepted: usize,
}

/// One model-scheduled follow-up and the input id its `agent_followup`
/// message carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Followup {
    pub(crate) text: String,
    pub(crate) input_id: String,
}

/// Shared control state for one runner. Undelivered follow-ups exist only for
/// the logical turn that accepted them; with a journal
/// (`TaskControl::set_followup_journal`) they also survive process loss for
/// that turn's automatic resume.
#[derive(Default)]
pub struct TaskControl {
    steering: Arc<Steering>,
    state: Mutex<State>,
    journal: OnceLock<Arc<dyn Mailbox>>,
    /// The journal behind the session's durable turn queue (`turns.json`).
    turn_queue_journal: OnceLock<Arc<dyn Mailbox>>,
}

pub(crate) struct Run {
    control: Arc<TaskControl>,
    generation: u64,
    /// Follow-ups of an earlier logical task that this run discarded,
    /// because that task will not be resumed.
    discarded: usize,
}

/// The error text of a follow-up whose durable copy could not be saved.
const FOLLOWUP_NOT_PERSISTED: &str = "the follow-up could not be saved and was not queued";

impl TaskControl {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(crate) fn steering(&self) -> Arc<Steering> {
        Arc::clone(&self.steering)
    }

    /// Whether a logical task is active (its `Run` is alive).
    pub(crate) fn is_active(&self) -> bool {
        self.lock().active.is_some()
    }
    pub(crate) fn authorization_scope(&self) -> Option<String> {
        self.lock()
            .active
            .as_ref()
            .map(|active| active.scope.clone())
    }

    /// Installs the durable turn-queue journal. A later call is ignored.
    pub fn set_turn_queue_journal(&self, journal: Arc<dyn Mailbox>) {
        let _ = self.turn_queue_journal.set(journal);
    }

    /// The journal of the durable turn queue, when the session has one.
    pub(crate) fn turn_queue_journal(&self) -> Option<Arc<dyn Mailbox>> {
        self.turn_queue_journal.get().cloned()
    }

    /// Installs the durable follow-up journal. A later call is ignored.
    pub(crate) fn set_followup_journal(&self, journal: Arc<dyn Mailbox>) {
        let _ = self.journal.set(journal);
    }

    /// Starts one logical turn. Approval retries reuse this guard explicitly.
    ///
    /// Journaled follow-ups of an earlier logical task are left only by
    /// process loss. When this run `resumes` that task (a recovery turn), it
    /// adopts them, in order, to run after its own step; otherwise they are
    /// discarded and counted in [`Run::discarded`].
    pub(crate) fn begin(
        self: &Arc<Self>,
        session_id: &str,
        original_task: &str,
    ) -> Result<Run, String> {
        self.begin_resuming(session_id, original_task, false)
    }

    /// [`Self::begin`] for a run that may resume an interrupted task.
    pub(crate) fn begin_resuming(
        self: &Arc<Self>,
        session_id: &str,
        original_task: &str,
        resumes: bool,
    ) -> Result<Run, String> {
        let mut state = self.lock();
        if state.active.is_some() {
            return Err("a task is already active".into());
        }
        let mut scope = crate::runtime::sidecar_mailbox::new_input_id();
        let mut followups = VecDeque::new();
        let mut discarded = 0;
        if let Some(journal) = self.journal.get() {
            for entry in journal.queued() {
                if resumes {
                    scope = entry.notification.scope.clone();
                    followups.push_back(Followup {
                        text: entry.notification.text,
                        input_id: entry.notification.id,
                    });
                } else {
                    journal.ack(entry.seq);
                    discarded += 1;
                }
            }
        }
        state.generation = state.generation.wrapping_add(1);
        state.active = Some(ActiveRun {
            session_id: session_id.to_string(),
            original_task: original_task.to_string(),
            current_task: original_task.to_string(),
            scope,
            accepted: followups.len(),
            followups,
        });
        Ok(Run {
            control: Arc::clone(self),
            generation: state.generation,
            discarded,
        })
    }

    /// Removes the journal entries of `scope`, or the one with `input_id`.
    fn forget(&self, scope: &str, input_id: Option<&str>) {
        let Some(journal) = self.journal.get() else {
            return;
        };
        for entry in journal.queued() {
            let matches = match input_id {
                Some(input_id) => entry.notification.id == input_id,
                None => entry.notification.scope == scope,
            };
            if matches {
                journal.ack(entry.seq);
            }
        }
    }

    pub(crate) fn original_task(&self, session_id: &str) -> Option<String> {
        self.lock()
            .active
            .as_ref()
            .filter(|active| active.session_id == session_id)
            .map(|active| active.original_task.clone())
    }

    pub(crate) fn current_task(&self, session_id: &str) -> Option<String> {
        self.lock()
            .active
            .as_ref()
            .filter(|active| active.session_id == session_id)
            .map(|active| active.current_task.clone())
    }

    pub(crate) fn steer_with_provenance(
        &self,
        session_id: &str,
        ticket: u64,
        text: &str,
        input_id: Option<&str>,
        provenance: Option<otto_core::model::InputProvenance>,
    ) -> Result<otto_core::agent::steering::Message, String> {
        let state = self.lock();
        if !state
            .active
            .as_ref()
            .is_some_and(|active| active.session_id == session_id)
        {
            return Err("the active task changed before guidance was accepted".into());
        }
        self.steering
            .submit_input_with_provenance(ticket, text, input_id, provenance)
            .map_err(submit_error)
    }

    pub(crate) fn queue_followup(&self, text: &str) -> Result<(), String> {
        let text = checked_text(text)?;
        let mut state = self.lock();
        let active = state
            .active
            .as_mut()
            .ok_or_else(|| "there is no active task to queue behind".to_string())?;
        if active.accepted >= MAX_FOLLOWUPS {
            return Err(format!(
                "the active task reached its {MAX_FOLLOWUPS} follow-up limit"
            ));
        }
        active.accepted += 1;
        let input_id = format!("followup:{}:{}", active.scope, active.accepted);
        if let Some(journal) = self.journal.get()
            && let Err(MailboxError::NotPersisted { seq, .. }) = journal.enqueue(Notification {
                kind: Some(NotificationKind::Message),
                text: text.to_string(),
                id: input_id.clone(),
                scope: active.scope.clone(),
                ..Notification::default()
            })
        {
            journal.ack(seq);
            active.accepted -= 1;
            return Err(FOLLOWUP_NOT_PERSISTED.into());
        }
        active.followups.push_back(Followup {
            text: text.to_string(),
            input_id,
        });
        Ok(())
    }
}

impl Run {
    pub(crate) fn set_current_task(&self, text: &str) {
        let mut state = self.control.lock();
        if state.generation == self.generation
            && let Some(active) = state.active.as_mut()
        {
            active.current_task = text.to_string();
        }
    }

    pub(crate) fn next_followup(&self) -> Option<Followup> {
        let mut state = self.control.lock();
        if state.generation != self.generation {
            return None;
        }
        state.active.as_mut()?.followups.pop_front()
    }

    /// Removes a follow-up from the journal once its step ran (its
    /// `agent_followup` message carries `followup.input_id`).
    pub(crate) fn followup_done(&self, followup: &Followup) {
        self.control.forget("", Some(&followup.input_id));
    }

    /// Puts `followup` back at the front, as an adopted journal entry would
    /// be after a restart.
    #[cfg(test)]
    pub(crate) fn requeue(&self, followup: Followup) {
        let mut state = self.control.lock();
        if state.generation == self.generation
            && let Some(active) = state.active.as_mut()
        {
            active.followups.push_front(followup);
        }
    }

    /// How many follow-ups of an earlier, unresumed task this run discarded.
    pub(crate) fn discarded(&self) -> usize {
        self.discarded
    }

    pub(crate) fn clear_followups(&self) -> usize {
        let mut state = self.control.lock();
        if state.generation == self.generation
            && let Some(active) = state.active.as_mut()
        {
            let count = active.followups.len();
            active.followups.clear();
            let scope = active.scope.clone();
            drop(state);
            self.control.forget(&scope, None);
            return count;
        }
        0
    }
}

impl Drop for Run {
    /// Every way a logical task ends discards its remaining follow-ups,
    /// journal included; only process loss skips this drop.
    fn drop(&mut self) {
        let mut state = self.control.lock();
        if state.generation == self.generation {
            let scope = state.active.take().map(|active| active.scope);
            drop(state);
            if let Some(scope) = scope {
                self.control.forget(&scope, None);
            }
        }
    }
}

pub(crate) fn checked_text(text: &str) -> Result<&str, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("text must contain non-whitespace characters".into());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!("text must be at most {MAX_TEXT_BYTES} UTF-8 bytes"));
    }
    Ok(text)
}

fn submit_error(error: SubmitError) -> String {
    match error {
        SubmitError::Inactive => "the active task has ended or is not accepting guidance".into(),
        SubmitError::Empty => "guidance must contain non-whitespace characters".into(),
        SubmitError::TooLong => format!("guidance must be at most {MAX_TEXT_BYTES} UTF-8 bytes"),
        SubmitError::Full => "the active task's guidance queue is full".into(),
        SubmitError::NotPersisted => INPUT_NOT_PERSISTED.into(),
    }
}

/// The error text of guidance whose durable copy could not be saved. The
/// guidance was not accepted; the HTTP API answers `503
/// input_not_persisted`.
pub const INPUT_NOT_PERSISTED: &str = "the input could not be saved and was not accepted";

/// The error text of an input whose `input_id` the session transcript
/// already holds: a client retry after the input was delivered. The HTTP API
/// answers `409 duplicate_input`.
pub const DUPLICATE_INPUT: &str = "an input with this input_id was already delivered";

#[derive(Default)]
pub(crate) struct ControlRoute(
    Mutex<Option<Route>>,
    Option<String>,
    Option<otto_core::model::InputProvenance>,
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    Steered,
    Queued,
    Replied,
    Failed(String),
}

impl ControlRoute {
    pub(crate) fn new(
        input_id: Option<String>,
        provenance: Option<otto_core::model::InputProvenance>,
    ) -> Self {
        Self(Mutex::new(None), input_id, provenance)
    }

    pub(crate) fn steer(
        &self,
        control: &TaskControl,
        session_id: &str,
        ticket: u64,
        text: &str,
    ) -> Result<bool, String> {
        let mut route = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if route.is_some() {
            return Ok(false);
        }
        match control.steer_with_provenance(session_id, ticket, text, self.1.as_deref(), self.2) {
            Ok(_) => {
                *route = Some(Route::Steered);
                Ok(true)
            }
            Err(error) => {
                *route = Some(Route::Failed(error.clone()));
                Err(error)
            }
        }
    }

    pub(crate) fn queue(&self) -> bool {
        let mut route = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if route.is_some() {
            return false;
        }
        *route = Some(Route::Queued);
        true
    }

    pub(crate) fn reply(&self) -> bool {
        let mut route = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if route.is_some() {
            return false;
        }
        *route = Some(Route::Replied);
        true
    }

    pub(crate) fn route(&self) -> Option<Route> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests;
