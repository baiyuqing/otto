//! Layer 2: the proposal generator over the labeled dataset with the profile model, scored
//! by `otto::memory::proposals::evaluation` and judged.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use otto::decision::{Backend, Exchange, Failure, Question};
use otto::memory::proposals::Generator;
use otto::memory::proposals::evaluation::{
    Generated, LabeledItem, Score, parse_labeled_items, score,
};
use otto_core::provider::Provider;
use serde::{Deserialize, Serialize};

use super::judging::{ProposalVerdict, judge_proposal};
use crate::eval_command::model_calls::{EvalEnvironment, complete_text};
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

    async fn ask(&self, state: &str, questions: &[Question], limit: Duration) -> Exchange {
        let user_text = otto::decision::session::user_text(state, questions);
        let call = complete_text(
            &self.env,
            otto::decision::session::SYSTEM_PROMPT,
            &user_text,
        );
        let reply = match tokio::time::timeout(limit, call).await {
            Err(_) => {
                return Exchange {
                    request: user_text,
                    response: None,
                    result: Err(Failure::Timeout),
                };
            }
            Ok(Err(message)) => {
                return Exchange {
                    request: user_text,
                    response: None,
                    result: Err(Failure::Error(message)),
                };
            }
            Ok(Ok(reply)) => reply,
        };
        let result = otto::decision::session::parse(&reply, questions);
        Exchange {
            request: user_text,
            response: Some(reply),
            result,
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

const LABELED_JSONL: &str =
    include_str!("../../../../testdata/memory-eval/proposals/labeled.jsonl");

/// The embedded labeled proposal dataset.
pub fn labeled_items() -> Result<Vec<LabeledItem>, String> {
    parse_labeled_items(LABELED_JSONL)
}

fn generated_record(generated: &Generated) -> GeneratedRecord {
    match generated {
        Generated::Proposal(proposal) => GeneratedRecord {
            action: Some(proposal.action.as_str().to_owned()),
            evidence_refs: proposal.evidence_refs.clone(),
            explanation: proposal.explanation.clone(),
            replacement: proposal.replacement.clone(),
            unavailable: None,
        },
        Generated::Unavailable(reason) => GeneratedRecord {
            unavailable: Some(reason.clone()),
            ..GeneratedRecord::default()
        },
    }
}

/// Runs the generator over every labeled item not yet in `proposals.jsonl`, scores and
/// judges each proposal, appends the rows and returns all rows.
pub async fn run_layer_two<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    out: &OutputDir,
    judge: bool,
    stdout: &mut (dyn Write + Send),
) -> Result<Vec<ProposalRecord>, String> {
    let mut rows: Vec<ProposalRecord> = out.read_jsonl(PROPOSALS_FILE)?;
    for item in labeled_items()? {
        if env.cancel.is_cancelled() {
            return Err("canceled".into());
        }
        if rows.iter().any(|row| row.name == item.name) {
            continue;
        }
        let generator = Generator::new(Box::new(EvalDecisionBackend { env: env.clone() }), None);
        let result = generator
            .generate(&item.input("memory-eval"), &env.cancel)
            .await;
        if env.cancel.is_cancelled() {
            return Err("canceled".into());
        }
        let generated = Generated::from_result(result);
        let mut row = ProposalRecord {
            name: item.name.clone(),
            category: item.category.clone(),
            generated: generated_record(&generated),
            score: score(&item, &generated),
            judge: None,
            judge_error: String::new(),
        };
        if judge && let Generated::Proposal(proposal) = &generated {
            match judge_proposal(env, &item, proposal).await {
                Ok(verdict) => row.judge = Some(verdict),
                Err(error) => row.judge_error = error,
            }
        }
        out.append_jsonl(PROPOSALS_FILE, &row)?;
        let outcome = row.generated.action.as_deref().unwrap_or("unavailable");
        writeln!(stdout, "layer 2 {}: {outcome}", item.name).map_err(|error| error.to_string())?;
        rows.push(row);
    }
    Ok(rows)
}
