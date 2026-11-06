//! The per-step memory refresh: before every provider step the agent asks the
//! binding for the lifecycle status of the records the conversation refers
//! to, drops recalled records that are no longer active (no re-retrieval),
//! and renders retraction notices for records that left the active state.

use std::collections::BTreeSet;

use super::memory::{RecordLifecycle, RecordStatus, render_memory_context};
use super::memory_retraction::{
    memory_request_text, render_retraction_notices, select_retraction_notices,
};
use super::overflow::RunDispatchState;
use super::{Agent, Event, EventSink};
use crate::model::Role;
use crate::operation::OperationControl;
use crate::provider::Provider;
use crate::session::{MemoryRecordRef, Session};
use crate::tool::ToolExecutor;

fn status_of<'a>(lifecycle: &'a [RecordLifecycle], id: &str) -> Option<&'a RecordStatus> {
    lifecycle
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| &entry.status)
}

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Refreshes `state.memory_context` and `state.retraction_context` for the
    /// next provider step. A lifecycle failure is a memory warning and keeps
    /// the previous request content.
    pub(super) async fn refresh_memory_context(
        &self,
        emit: EventSink<'_>,
        state: &mut RunDispatchState,
        control: &dyn OperationControl,
    ) {
        let Some(binding) = &self.options.memory else {
            return;
        };
        let referenced = self.referenced_memory_records(state);
        let mut ids: BTreeSet<String> = referenced
            .iter()
            .map(|reference| reference.id.clone())
            .collect();
        ids.extend(state.recalled.iter().map(|record| record.id.clone()));
        if ids.is_empty() {
            return;
        }
        let ids: Vec<String> = ids.into_iter().collect();
        let lifecycle = match binding.lifecycle(&ids, control.cancellation_token()).await {
            Ok(lifecycle) => lifecycle,
            Err(error) => {
                emit(Event::MemoryWarning {
                    message: error.to_string(),
                });
                return;
            }
        };
        let active: Vec<_> = state
            .recalled
            .iter()
            .filter(|record| {
                status_of(&lifecycle, &record.id).is_none_or(|status| {
                    matches!(status, RecordStatus::Active | RecordStatus::Unknown)
                })
            })
            .cloned()
            .collect();
        state.memory_context = self.redactor.redact_string(&render_memory_context(&active));
        state.memory_record_ids = active.iter().map(|record| record.id.clone()).collect();
        let notices = select_retraction_notices(&referenced, &lifecycle);
        state.retraction_context = self
            .redactor
            .redact_string(&render_retraction_notices(&notices));
        *self
            .last_memory_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            memory_request_text(&state.memory_context, &state.retraction_context);
    }

    /// The record references still in the model view: the persisted
    /// references of earlier user turns whose message is in the model
    /// messages, the latest checkpoint's references, and the records this
    /// turn already sent in an earlier step.
    fn referenced_memory_records(&self, state: &RunDispatchState) -> Vec<MemoryRecordRef> {
        let model_messages = self.session.model_messages();
        let current_turn = model_messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.id.clone());
        let visible: BTreeSet<&str> = model_messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.id.as_str())
            .collect();
        let mut referenced: Vec<MemoryRecordRef> = Vec::new();
        for turn in self.session.memory_references() {
            if Some(&turn.user_message_id) == current_turn.as_ref()
                || !visible.contains(turn.user_message_id.as_str())
            {
                continue;
            }
            referenced.extend(turn.records);
        }
        if let Some(latest) = self.session.latest_compaction() {
            referenced.extend(latest.details.memory_records);
        }
        referenced.extend(
            state
                .recalled
                .iter()
                .filter(|record| state.sent_record_ids.contains(&record.id))
                .map(|record| MemoryRecordRef {
                    id: record.id.clone(),
                    revision: record.revision.clone(),
                }),
        );
        referenced
    }
}
