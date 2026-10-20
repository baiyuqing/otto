//! Runner handling of user authorization records.

use super::*;

impl Runner {
    /// Keep temporary output across all provider steps of one logical turn,
    /// including approval retries and queued follow-ups.
    pub(crate) fn retain_temporary_outputs(&self) -> crate::tool::bash_output::LogLease {
        self.output_logs.lease()
    }

    pub(crate) fn set_scheduled_authorization_run(&self, label: &str) {
        let mut run = self.authorization_run.lock().expect("authorization run");
        if !run.starts_with("schedule:") {
            *run = format!("schedule:{label}");
        }
    }
    pub(crate) async fn install_authorization_context(
        &self,
        mut context: crate::approval::reviewer::ReviewContext,
    ) -> Result<(), String> {
        let Some((service, scope)) = &self.approvals else {
            return Ok(());
        };
        if scope.session_id.is_empty() {
            return Ok(());
        }
        let mut current = self.review_context.lock().await;
        context.policy_digest = current.policy_digest.clone();
        context.refresh_digest();
        self.session()
            .append_custom(
                crate::approval::reviewer::CONTEXT_CUSTOM_TYPE,
                &serde_json::to_string(&context).map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        service
            .set_review_context(&scope.workspace, &scope.session_id, context.clone())
            .map_err(|error| error.to_string())?;
        *current = context;
        Ok(())
    }
    /// Captures only text admitted through a human input boundary, before any
    /// model/tool execution. Generated child, wake and scheduled prompts never call this.
    pub(crate) async fn record_user_authorization(&self, text: &str) -> Result<(), AgentError> {
        self.record_user_authorizations(&[text.to_owned()]).await
    }

    async fn record_user_authorizations(&self, texts: &[String]) -> Result<(), AgentError> {
        let Some((service, scope)) = &self.approvals else {
            return Ok(());
        };
        if scope.session_id.is_empty() {
            return Ok(());
        }
        let mut current = self.review_context.lock().await;
        let mut context = current.clone();
        if context
            .instructions
            .iter()
            .any(|instruction| instruction == crate::approval::reviewer::INCOMPLETE_EVIDENCE)
        {
            context.trusted_user_messages.clear();
            context.instructions.retain(|instruction| {
                instruction != crate::approval::reviewer::INCOMPLETE_EVIDENCE
            });
            context
                .instructions
                .push(crate::approval::reviewer::FRESH_EVIDENCE_ONLY.into());
        }
        context
            .trusted_user_messages
            .extend(texts.iter().map(|text| self.redact_text(text)));
        // Never select an arbitrary excerpt which could drop a prohibition.
        if serde_json::to_string(&context)
            .map_err(|error| AgentError::Other(error.to_string()))?
            .len()
            > crate::approval::reviewer::MAX_INPUT_BYTES
        {
            context.trusted_user_messages.clear();
            context.instructions.clear();
            context
                .instructions
                .push(crate::approval::reviewer::INCOMPLETE_EVIDENCE.into());
        }
        context.refresh_digest();
        let unavailable = crate::approval::reviewer::ReviewContext {
            evidence_digest: context.evidence_digest.clone(),
            policy_digest: "authorization-persistence-pending".into(),
            ..Default::default()
        };
        service
            .set_review_context(&scope.workspace, &scope.session_id, unavailable)
            .map_err(|error| AgentError::Other(error.to_string()))?;
        self.session()
            .append_custom(
                crate::approval::reviewer::CONTEXT_CUSTOM_TYPE,
                &serde_json::to_string(&context)
                    .map_err(|error| AgentError::Other(error.to_string()))?,
            )
            .await
            .map_err(|error| AgentError::Other(error.to_string()))?;
        {
            let pending = self
                .pending_authorization
                .lock()
                .expect("pending authorization");
            if pending.is_empty() {
                service
                    .set_review_context(&scope.workspace, &scope.session_id, context.clone())
                    .map_err(|error| AgentError::Other(error.to_string()))?;
            }
        }
        *current = context;
        Ok(())
    }

    /// Sync admission invalidates a review immediately. The monitor persists
    /// the original input before installing an assessable replacement context.
    pub(crate) fn queue_user_authorization(&self, text: &str) -> Result<(), String> {
        let Some((service, scope)) = &self.approvals else {
            return Ok(());
        };
        if scope.session_id.is_empty() {
            return Ok(());
        }
        let mut pending = self
            .pending_authorization
            .lock()
            .expect("pending authorization");
        pending.push(self.redact_text(text));
        let unavailable = crate::approval::reviewer::ReviewContext {
            evidence_digest: crate::approval::digest(&pending.join("\n")),
            policy_digest: "authorization-input-pending".into(),
            ..Default::default()
        };
        service
            .set_review_context(&scope.workspace, &scope.session_id, unavailable)
            .map_err(|error| error.to_string())
    }

    pub(super) async fn flush_user_authorization(&self) -> Result<(), AgentError> {
        let pending = std::mem::take(
            &mut *self
                .pending_authorization
                .lock()
                .expect("pending authorization"),
        );
        if pending.is_empty() {
            Ok(())
        } else {
            self.record_user_authorizations(&pending).await
        }
    }
}
