//! Isolated, tool-less authorization assessment. The authority service owns decisions.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use otto_core::approval::{Request, ReviewAssessment};

pub(crate) const CONTEXT_CUSTOM_TYPE: &str = "otto.authorization";
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
pub(crate) const INCOMPLETE_EVIDENCE: &str =
    "Authorization evidence is incomplete; require human review.";
pub(crate) const STANDING_CONSTRAINTS_UNAVAILABLE: &str =
    "Accepted standing user constraints could not be read; require human review.";
pub(crate) const FRESH_EVIDENCE_ONLY: &str = "Historical authorization evidence is unavailable. Use only the newly \
    admitted original user request for this task. If the operation relies \
    on earlier scope, prior decisions or omitted constraints, ask the user. \
    An independent, concretely authorized new task may be assessed using \
    its new evidence.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewContext {
    pub trusted_user_messages: Vec<String>,
    pub instructions: Vec<String>,
    pub evidence_digest: String,
    pub policy_digest: String,
}

impl ReviewContext {
    pub fn refresh_digest(&mut self) {
        self.evidence_digest = super::digest(
            &serde_json::json!({"user":self.trusted_user_messages,"instructions":self.instructions})
                .to_string(),
        );
    }
}

#[async_trait::async_trait]
pub trait ReviewProvider: Send + Sync {
    async fn assess(
        &self,
        request: &Request,
        context: &ReviewContext,
        cancel: &CancellationToken,
    ) -> std::result::Result<ReviewAssessment, String>;
}
