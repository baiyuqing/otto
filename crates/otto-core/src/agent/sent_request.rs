//! Keeps the last ordinary provider request for the context report.

use super::context_report::{SentRequest, sent_sections};
use super::memory_retraction::memory_request_text;
use super::{Agent, Arc, RunDispatchState};
use crate::provider::{Provider, Request};
use crate::session::Session;
use crate::tool::ToolExecutor;

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Stores the sections of `guarded`, the request that is handed to the
    /// provider after provider hooks and the redaction guard. The pre-guard
    /// request is never stored. The text stays in this agent's memory: it is
    /// not logged, emitted or appended to the session.
    pub(super) fn record_sent_request(&self, guarded: &Request, state: &RunDispatchState) {
        let memory_text = memory_request_text(&state.memory_context, &state.retraction_context);
        let sent = SentRequest {
            sent_at: (self.options.now)().timestamp_millis(),
            sections: sent_sections(guarded, &self.options.system_prompt_parts, &memory_text),
        };
        *self
            .last_sent_request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(sent));
    }
}
