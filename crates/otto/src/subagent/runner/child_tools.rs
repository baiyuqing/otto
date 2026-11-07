//! The child transcript handle and the tool view a child receives.

use super::*;

/// Shares one child transcript between the child's agent and the runner, which
/// reads it back to build the final report.
pub type Transcript = Arc<dyn Session + Send + Sync>;

/// Builds the transcript for the task with the given id and returns it with
/// its file path. The second argument is the fresh session ID to use in its
/// header. `Ok(None)` keeps the child in memory; an error fails the
/// task before it starts.
pub type ChildSession =
    Arc<dyn Fn(&str, &str) -> Result<Option<(Transcript, String)>, String> + Send + Sync>;

/// Builds the durable mailbox for the child whose transcript is at the given
/// path, restoring the parent messages it had not read when it stopped.
pub type ChildMailbox = Arc<dyn Fn(&str) -> Arc<dyn Mailbox> + Send + Sync>;

pub(super) struct SharedTranscript(pub(super) Transcript);

#[async_trait::async_trait]
impl Session for SharedTranscript {
    fn session_id(&self) -> Option<String> {
        self.0.session_id()
    }

    fn memory_references(&self) -> Vec<otto_core::session::TurnMemoryReferences> {
        self.0.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.0.messages()
    }

    fn model_messages(&self) -> Vec<Message> {
        self.0.model_messages()
    }

    async fn append(&self, message: Message) -> Result<(), otto_core::session::SessionError> {
        self.0.append(message).await
    }

    fn latest_compaction(&self) -> Option<otto_core::session::CompactionMetadata> {
        self.0.latest_compaction()
    }

    fn remote_compaction(&self) -> Option<otto_core::session::RemoteCompaction> {
        self.0.remote_compaction()
    }

    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        self.0.post_remote_compaction_messages()
    }

    async fn append_remote_compaction(
        &self,
        checkpoint: otto_core::session::RemoteCompaction,
    ) -> Result<(), otto_core::session::SessionError> {
        self.0.append_remote_compaction(checkpoint).await
    }

    async fn append_compaction(
        &self,
        checkpoint: otto_core::session::CompactionCheckpoint,
    ) -> Result<otto_core::session::CompactionMetadata, otto_core::session::SessionError> {
        self.0.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), otto_core::session::SessionError> {
        self.0.append_operation_fact(fact).await
    }

    fn operation_history(&self) -> Vec<otto_core::session::OperationFact> {
        self.0.operation_history()
    }

    fn operation_ledger(&self) -> otto_core::session::OperationLedger {
        self.0.operation_ledger()
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        self.0.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        self.0.interrupted_turn()
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.0.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: otto_core::model::Message,
    ) -> Result<otto_core::session::AppendOutcome, otto_core::session::SessionError> {
        self.0.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.0.has_input(input_id)
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), otto_core::session::SessionError> {
        self.0.append_custom(custom_type, data).await
    }
}

/// One child's view of the shared child registry: the tools whose names are in
/// `allowed`, in the registry's own order. `None` allows every child tool.
pub struct ChildTools {
    pub(super) registry: Arc<Registry>,
    pub(super) allowed: Option<BTreeSet<String>>,
    pub(super) write_policy: WritePolicy,
    pub(super) write_paths: Vec<String>,
    pub(super) parent_inbox: Arc<dyn Mailbox>,
    pub(super) task_id: String,
    pub(super) session_id: String,
    pub(super) max_output_bytes: usize,
    /// Remaining `agent_report` calls; see [`REPORT_BUDGET`] and
    /// [`ChildTools::report`].
    pub(super) reports_left: Arc<AtomicUsize>,
}

impl ChildTools {
    pub(super) fn permits(&self, name: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }

    fn permits_write_tool(&self, name: &str, arguments: &RawValue) -> Result<(), String> {
        if !matches!(name, "write" | "edit") {
            return Ok(());
        }
        match self.write_policy {
            WritePolicy::ReadOnly | WritePolicy::ProposeOnly => Err(format!(
                "write_policy {} denies workspace mutation tool {name}",
                self.write_policy.as_str()
            )),
            WritePolicy::SingleWriter => Ok(()),
            WritePolicy::OwnedPaths => {
                let path = mutation_path(arguments)?;
                if self
                    .write_paths
                    .iter()
                    .any(|pattern| path_matches(pattern, &path))
                {
                    Ok(())
                } else {
                    Err(format!(
                        "write_policy owned_paths denies {name} for path {path:?}"
                    ))
                }
            }
        }
    }

    /// Child-only tool that sends one message to the parent without ending
    /// this sub-agent task. Use only to answer a `[parent-message]` question
    /// or to report a blocker needing a parent decision, never for progress
    /// updates, plans, or interim findings. Budgeted by [`REPORT_BUDGET`]; see
    /// [`ChildTools::report`].
    fn agent_report_definition() -> ToolDefinition {
        let description = format!(
            "Send a message to the parent agent without ending this sub-agent task. \
                Use it only to answer a question the parent sent as a [parent-message], \
                or to report a blocker that needs a parent decision while you keep \
                working on other parts of the task. Do not use it for progress updates, \
                plans, raw logs, source dumps or interim findings. Keep the answer or \
                blocker within 4 KiB; the parent transport truncates longer reports. \
                For longer answers, write a persistent report when permitted and send \
                its summary, path and section index; this call does not automatically \
                save overflow. Limited to {REPORT_BUDGET} reports per task on your own \
                initiative, plus one more for each [parent-message] received; a call \
                past the limit returns an error instead of reaching the parent. A \
                report that cannot be saved to the parent's pending-notification file \
                also returns an error and does not reach the parent."
        );
        definition(
            "agent_report",
            &description,
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The answer to the parent's question, or the blocker and the decision it needs."
                    }
                },
                "required": ["message"]
            }),
        )
    }

    /// Pushes `message` to the parent's inbox, consuming one report from
    /// [`REPORT_BUDGET`]. The caller must check and decrement `reports_left`
    /// before calling this; see [`ChildTools::execute`].
    fn report(&self, message: &str) -> ToolResult {
        let text = capped_text_result(
            &format!("[task-report] task {}\n{}", self.task_id, message),
            self.max_output_bytes.min(PARENT_REPORT_BYTES),
        )
        .content;
        if let Err(otto_core::agent::inbox::MailboxError::NotPersisted { seq, reason }) =
            self.parent_inbox.enqueue(Notification {
                task_id: self.task_id.clone(),
                kind: Some(NotificationKind::TaskReport),
                text,
                usage: None,
                ..Default::default()
            })
        {
            self.parent_inbox.ack(seq);
            return error_result(format!("report not sent: {reason}"));
        }
        text_result("report sent to parent")
    }
}

fn mutation_path(arguments: &RawValue) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(arguments.get())
        .map_err(|_| "invalid tool arguments for write policy".to_string())?;
    let path = value
        .get("path")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "missing required argument: path".to_string())?;
    Ok(path.to_string())
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path
            .strip_prefix(&format!("{prefix}/"))
            .is_some_and(|rest| !rest.contains('/'));
    }
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        return path.starts_with(prefix) && path.ends_with(suffix);
    }
    pattern == path
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentReportArgs {
    #[serde(default)]
    message: String,
}

pub(crate) const AGENT_REPORT_RETRY: (&str, otto_core::model::RetrySafety) =
    ("agent_report", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl ToolExecutor for ChildTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<ToolDefinition> = self
            .registry
            .definitions()
            .into_iter()
            .filter(|definition| self.permits(&definition.name))
            .collect();
        if self.permits("session_info") {
            definitions.push(
                crate::tool::session_info::SessionInfo {
                    session_id: self.session_id.clone(),
                }
                .definition(),
            );
        }
        definitions.push(Self::agent_report_definition());
        definitions
    }

    fn retry_safety(&self, name: &str) -> otto_core::model::RetrySafety {
        if name == "session_info" && self.permits(name) {
            return otto_core::model::RetrySafety::ReadOnly;
        }
        if name == AGENT_REPORT_RETRY.0 {
            return AGENT_REPORT_RETRY.1;
        }
        if self.permits(name) {
            self.registry.retry_safety(name)
        } else {
            otto_core::model::RetrySafety::NonIdempotent
        }
    }

    fn batch_control(
        &self,
        name: &str,
        execution: &ToolExecution,
    ) -> otto_core::approval::BatchControl {
        self.registry.batch_control(name, execution)
    }

    async fn execute(&self, call: ToolCall<'_>, control: &dyn OperationControl) -> ToolExecution {
        let local = |mut result: ToolResult| {
            let outcome =
                result
                    .outcome_override
                    .take()
                    .unwrap_or(otto_core::model::OperationOutcome {
                        disposition: if result.is_error {
                            otto_core::model::OperationDisposition::Error
                        } else {
                            otto_core::model::OperationDisposition::Succeeded
                        },
                        effect_certainty: if result.is_error {
                            otto_core::model::EffectCertainty::NotStarted
                        } else {
                            otto_core::model::EffectCertainty::Completed
                        },
                        stop_reason: None,
                    });
            ToolExecution { result, outcome }
        };
        if call.name == "session_info" && self.permits(call.name) {
            if control.admission_stop_reason().is_some() {
                return local(error_result(CONTEXT_CANCELED));
            }
            return local(
                crate::tool::session_info::SessionInfo {
                    session_id: self.session_id.clone(),
                }
                .execute(call.arguments, control.cancellation_token())
                .await,
            );
        }
        if call.name == "agent_report" {
            match control.admission_stop_reason() {
                Some(OperationStopReason::Deadline) => {
                    return local(
                        error_result("operation deadline exceeded").deadline_not_started(),
                    );
                }
                Some(_) => {
                    return local(error_result(CONTEXT_CANCELED).cancelled_not_started());
                }
                None => {}
            }
            let args: AgentReportArgs = match decode_strict_json(call.arguments.get(), &["message"])
            {
                Ok(args) => args,
                Err(message) => return local(error_result(message).not_started()),
            };
            let message = args.message.trim();
            if message.is_empty() {
                return local(error_result("message is required").not_started());
            }
            if self
                .reports_left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_err()
            {
                return local(
                    error_result(
                        "agent_report limit used up for this task; put this in your final report instead. Each [parent-message] from the parent allows one more report.",
                    )
                    .not_started(),
                );
            }
            return local(self.report(message));
        }
        if !self.permits(call.name) {
            return local(ToolResult::unknown_tool(call.name));
        }
        if let Err(message) = self.permits_write_tool(call.name, call.arguments) {
            return local(crate::tool::error_result(message).not_started());
        }
        if let Some((service, mut scope, _)) = self.registry.approval_binding() {
            scope.task_id = self.task_id.clone();
            if call.name == "request_approval" {
                return local(
                    crate::tool::approval::RequestApproval {
                        service,
                        scope,
                        turn_origin: None,
                    }
                    .execute_call(call, control.cancellation_token())
                    .await,
                );
            }
            if matches!(call.name, "approval_pending" | "approval_revoke") {
                return local(
                    crate::tool::approval::Control {
                        service,
                        scope,
                        revoke: call.name == "approval_revoke",
                        expected_id: None,
                        legacy: None,
                    }
                    .execute(call.arguments, control.cancellation_token())
                    .await,
                );
            }
            crate::workflow::provenance::non_parent(self.registry.execute_with_scope(
                call,
                control,
                Some(&scope),
            ))
            .await
        } else {
            crate::workflow::provenance::non_parent(self.registry.execute(call, control)).await
        }
    }
}
