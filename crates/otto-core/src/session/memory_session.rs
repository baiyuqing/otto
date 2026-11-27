//! In-memory session implementation.

use std::sync::Mutex;

use super::*;

/// An in-memory transcript. Used by tests and by sub-agents that must not
/// write to disk.
#[derive(Debug, Default)]
pub struct MemorySession {
    session_id: Option<String>,
    state: Mutex<State>,
}

impl MemorySession {
    /// Creates an ephemeral transcript bound to its runner's stable identity.
    pub fn with_session_id(session_id: String) -> Self {
        Self {
            session_id: Some(session_id),
            ..Self::default()
        }
    }

    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl Session for MemorySession {
    fn session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    fn messages(&self) -> Vec<Message> {
        self.state.lock().expect("session mutex").messages.clone()
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        message.validate()?;
        let mut state = self.state.lock().expect("session mutex");
        state.append(message)
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        mut message: Message,
    ) -> Result<AppendOutcome, SessionError> {
        message.input_id = input_id.to_owned();
        message.validate()?;
        let mut state = self.state.lock().expect("session mutex");
        if state.seen_inputs.contains(input_id) {
            return Ok(AppendOutcome::AlreadyPresent);
        }
        state.append(message)?;
        Ok(AppendOutcome::Appended)
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.state
            .lock()
            .expect("session mutex")
            .seen_inputs
            .contains(input_id)
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.state
            .lock()
            .expect("session mutex")
            .latest_compaction
            .clone()
    }

    fn remote_compaction(&self) -> Option<RemoteCompaction> {
        self.state
            .lock()
            .expect("session mutex")
            .remote_compaction
            .clone()
    }

    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        let state = self.state.lock().expect("session mutex");
        if state.remote_compaction.is_some() {
            state.post_remote_compaction_messages.clone()
        } else {
            state.messages.clone()
        }
    }

    async fn append_remote_compaction(
        &self,
        checkpoint: RemoteCompaction,
    ) -> Result<(), SessionError> {
        validate_remote_compaction(&checkpoint)
            .map_err(|error| SessionError::Persist(error.to_string()))?;
        let mut state = self.state.lock().expect("session mutex");
        state.remote_compaction = Some(checkpoint);
        state.post_remote_compaction_messages.clear();
        Ok(())
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        compaction::validate_compaction_checkpoint(&checkpoint)
            .map_err(|error| SessionError::Persist(error.to_string()))?;
        let mut state = self.state.lock().expect("session mutex");
        let Some(first_kept) = state
            .messages
            .iter()
            .position(|message| message.id == checkpoint.first_kept_entry_id)
        else {
            return Err(SessionError::Sequence(
                "compaction first-kept message is not in the active context",
            ));
        };
        state.checkpoint_counter += 1;
        let checkpoint_id = format!("compaction-{}", state.checkpoint_counter);

        let mut context = context::new_context_message(
            &checkpoint_id,
            context::COMPACTION_CONTEXT_TYPE,
            true,
            format!("[Compaction summary]\n{}", checkpoint.summary),
            checkpoint.created_at,
            checkpoint.usage,
        );
        context.context_tokens_before = checkpoint.tokens_before;

        let mut candidate = Vec::with_capacity(1 + state.messages.len() - first_kept);
        candidate.push(context);
        candidate.extend_from_slice(&state.messages[first_kept..]);
        let replayed = State::replay(&candidate)?;

        let metadata = CompactionMetadata {
            id: checkpoint_id.clone(),
            summary: checkpoint.summary,
            first_kept_entry_id: checkpoint.first_kept_entry_id,
            tokens_before: checkpoint.tokens_before,
            usage: checkpoint.usage,
            details: checkpoint.details,
            retained_tail_only: false,
            first_post_checkpoint_message_id: String::new(),
        };
        state.messages = replayed.messages;
        state.pending = replayed.pending;
        state.seen_call_ids = replayed.seen_call_ids;
        state.seen_ids.insert(checkpoint_id);
        state.latest_compaction = Some(metadata.clone());
        // The newest checkpoint kind wins, as in the persistent context
        // projection: a local summary supersedes an older opaque prefix.
        state.remote_compaction = None;
        state.post_remote_compaction_messages.clear();
        Ok(metadata)
    }

    async fn append_operation_fact(&self, fact: OperationFact) -> Result<(), SessionError> {
        let mut state = self.state.lock().expect("session mutex");
        let mut candidate = state.operation_ledger.clone();
        candidate.apply(fact.clone())?;
        state.operation_ledger = candidate;
        state.operation_facts.push(fact);
        Ok(())
    }

    fn operation_history(&self) -> Vec<OperationFact> {
        self.state
            .lock()
            .expect("session mutex")
            .operation_facts
            .clone()
    }

    fn operation_ledger(&self) -> OperationLedger {
        self.state
            .lock()
            .expect("session mutex")
            .operation_ledger
            .clone()
    }

    fn observed_capabilities(&self) -> Option<Capabilities> {
        self.state
            .lock()
            .expect("session mutex")
            .observed_capabilities
            .clone()
    }

    fn interrupted_turn(&self) -> Option<InterruptedTurn> {
        self.state
            .lock()
            .expect("session mutex")
            .interrupted_turn
            .clone()
    }

    fn goal(&self) -> Option<crate::goal::Goal> {
        self.state.lock().expect("session mutex").goal.clone()
    }

    async fn compare_and_set_goal(
        &self,
        expected: Option<&crate::goal::Goal>,
        goal: &crate::goal::Goal,
    ) -> Result<bool, SessionError> {
        if !goal.is_valid() {
            return Err(SessionError::Persist("malformed otto.goal data".into()));
        }
        let mut state = self.state.lock().expect("session mutex");
        if state.goal.as_ref() != expected {
            return Ok(false);
        }
        state.goal = Some(goal.clone());
        Ok(true)
    }

    fn plan(&self) -> Vec<PlanStep> {
        self.state.lock().expect("session mutex").plan.clone()
    }

    fn memory_references(&self) -> Vec<TurnMemoryReferences> {
        self.state
            .lock()
            .expect("session mutex")
            .memory_references
            .clone()
    }

    async fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError> {
        if custom_type == MEMORY_RECALL_CUSTOM_TYPE {
            let record = decode_memory_recall(data)
                .ok_or_else(|| SessionError::Persist("malformed otto.memory_recall data".into()))?;
            let mut state = self.state.lock().expect("session mutex");
            let user_message_id = state
                .messages
                .iter()
                .rev()
                .find(|message| message.role == Role::User)
                .map(|message| message.id.clone())
                .ok_or(SessionError::Sequence(
                    "memory recall record has no preceding user message",
                ))?;
            state.memory_references.push(TurnMemoryReferences {
                user_message_id,
                records: record.records,
            });
            return Ok(());
        }
        if custom_type == TURN_CUSTOM_TYPE {
            let record = decode_turn_record(data)
                .ok_or_else(|| SessionError::Persist("malformed otto.turn data".into()))?;
            turn::apply_turn_record(
                &mut self.state.lock().expect("session mutex").interrupted_turn,
                &record,
            );
            return Ok(());
        }
        if custom_type == CAPABILITIES_CUSTOM_TYPE {
            let capabilities = decode_capabilities(data)
                .ok_or_else(|| SessionError::Persist("malformed otto.capabilities data".into()))?;
            self.state
                .lock()
                .expect("session mutex")
                .observed_capabilities = Some(capabilities);
            return Ok(());
        }
        if custom_type == crate::goal::GOAL_CUSTOM_TYPE {
            let goal: crate::goal::Goal = serde_json::from_str(data)
                .map_err(|_| SessionError::Persist("malformed otto.goal data".into()))?;
            if !goal.is_valid() {
                return Err(SessionError::Persist("malformed otto.goal data".into()));
            }
            self.state.lock().expect("session mutex").goal = Some(goal);
            return Ok(());
        }
        if custom_type == PLAN_CUSTOM_TYPE {
            let steps = decode_plan(data)
                .ok_or_else(|| SessionError::Persist("malformed otto.plan data".into()))?;
            self.state.lock().expect("session mutex").plan = steps;
            return Ok(());
        }
        if custom_type == REMOTE_COMPACTION_CUSTOM_TYPE {
            let checkpoint = decode_remote_compaction(data)
                .map_err(|error| SessionError::Persist(error.to_string()))?;
            let mut state = self.state.lock().expect("session mutex");
            state.remote_compaction = Some(checkpoint);
            state.post_remote_compaction_messages.clear();
            return Ok(());
        }
        if custom_type != OPERATION_CUSTOM_TYPE {
            return Ok(());
        }
        match operation::decode_operation_fact(data)? {
            DecodedOperationFact::Fact(fact) => self.append_operation_fact(fact).await,
            DecodedOperationFact::Unsupported { .. } => Ok(()),
        }
    }
}
