//! Layer 2: the proposal generator over the labeled dataset with the profile model, scored
//! by `otto::memory::proposals::evaluation` and judged.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use otto::decision::{Backend, Exchange, Question};
use otto::memory::proposals::evaluation::Score;
use otto_core::provider::Provider;
use serde::{Deserialize, Serialize};

use super::judging::ProposalVerdict;
use crate::eval_command::model_calls::EvalEnvironment;
use crate::eval_command::output_files::OutputDir;

pub const PROPOSALS_FILE: &str = "proposals.jsonl";

/// A `decision::Backend` over the evaluation environment: the profile model through
/// `complete_text`, the session-backend prompt and parser.
pub struct EvalDecisionBackend<P> {
    pub env: Arc<EvalEnvironment<P>>,
}

#[async_trait::async_trait]
impl<P: Provider + Clone + Send + Sync + 'static> Backend for EvalDecisionBackend<P> {
    fn model(&self) -> String {
        self.env.profile_model.clone()
    }

    fn auxiliary(&self) -> bool {
        false
    }

    async fn ask(&self, _state: &str, _questions: &[Question], _limit: Duration) -> Exchange {
        Exchange {
            request: String::new(),
            response: None,
            result: Err(otto::decision::Failure::Error("not implemented".into())),
        }
    }
}

/// The raw proposal of one labeled item, or why there is none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedRecord {
    pub action: Option<String>,
    pub evidence_refs: Vec<String>,
    pub explanation: String,
    pub replacement: Option<String>,
    pub unavailable: Option<String>,
}

/// One row of `proposals.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalRecord {
    pub name: String,
    pub category: String,
    pub generated: GeneratedRecord,
    pub score: Score,
    #[serde(default)]
    pub judge: Option<ProposalVerdict>,
    #[serde(default)]
    pub judge_error: String,
}

/// Runs the generator over every labeled item not yet in `proposals.jsonl`, scores and
/// judges each proposal, appends the rows and returns all rows.
pub async fn run_layer_two<P: Provider + Clone + Send + Sync + 'static>(
    _env: &Arc<EvalEnvironment<P>>,
    _out: &OutputDir,
    _judge: bool,
    _stdout: &mut (dyn Write + Send),
) -> Result<Vec<ProposalRecord>, String> {
    Err("not implemented".into())
}
