//! `submit_verification`: validate a verdict against typed persisted results.
//!
//! The tool is parent-only because child registries are assembled before a
//! child session exists and therefore cannot be authoritatively bound to that
//! child's transcript. It reads the parent's current append-only transcript;
//! it never executes commands or inspects result text.

use otto_core::model::{BlockType, Message, ToolDefinition};
use otto_core::session::Session as _;
use otto_core::tool::ToolResult;
use otto_core::verification::{EvidenceFact, Submission, validate};
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::decode_strict_json;
use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};
use crate::runtime::builder::SharedSession;

const DESCRIPTION: &str = "Validate a verification verdict against prior, durably persisted typed \
    ToolResult blocks in this current session. This tool does not run \
    commands, inspect output meaning, infer command families, judge \
    coverage, or gate completion. claim is nonblank and at most 2000 \
    characters; evidence has at most 32 unique tool_call_id items; \
    limitations has at most 16 nonblank items of at most 1000 characters \
    each. Success returns the accepted verdict as canonical JSON; invalid \
    submissions return a tool error.";

pub fn verification_definition() -> ToolDefinition {
    definition(
        "submit_verification",
        DESCRIPTION,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "claim": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 2000,
                    "description": "Claim being assessed; nonblank, at most 2000 characters."
                },
                "verdict": {
                    "type": "string",
                    "enum": ["passed", "failed", "blocked", "not_run", "inconclusive"],
                    "description": "Proposed claim verdict. Evidence-integrity rules determine whether Otto accepts it."
                },
                "evidence": {
                    "type": "array",
                    "maxItems": 32,
                    "description": "Prior persisted typed tool results. IDs must be unique; context does not satisfy a verdict's required relation.",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "tool_call_id": {
                                "type": "string",
                                "minLength": 1,
                                "description": "ID of a prior, durably persisted typed tool result in this session."
                            },
                            "relation": {
                                "type": "string",
                                "enum": ["supports", "contradicts", "blocker", "context"],
                                "description": "How the cited result relates to the claim; context does not satisfy a verdict's required relation."
                            }
                        },
                        "required": ["tool_call_id", "relation"]
                    }
                },
                "limitations": {
                    "type": "array",
                    "maxItems": 16,
                    "description": "Known limitations; each must be nonblank and at most 1000 characters.",
                    "items": {"type": "string", "minLength": 1, "maxLength": 1000}
                }
            },
            "required": ["claim", "verdict", "evidence", "limitations"]
        }),
    )
}

pub struct SubmitVerification {
    session: SharedSession,
}

impl SubmitVerification {
    pub fn new(session: SharedSession) -> Self {
        Self { session }
    }
}

// Submission participates in a verification workflow; do not automatically
// resubmit a verdict after an uncertain interruption, even if validation reads history.
pub(crate) const SUBMIT_VERIFICATION_RETRY: (&str, otto_core::model::RetrySafety) = (
    "submit_verification",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait::async_trait]
impl Tool for SubmitVerification {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        SUBMIT_VERIFICATION_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        verification_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        submit(arguments, &self.session.messages())
    }
}

fn submit(arguments: &RawValue, messages: &[Message]) -> ToolResult {
    let submission: Submission = match decode_strict_json(arguments.get(), &[]) {
        Ok(submission) => submission,
        Err(error) => return error_result(error),
    };
    match validate(submission, &persisted_facts(messages)) {
        Ok(accepted) => match serde_json::to_string(&accepted) {
            Ok(text) => text_result(text),
            Err(error) => error_result(format!("could not encode accepted verdict: {error}")),
        },
        Err(error) => error_result(format!("invalid verification submission: {error}")),
    }
}

fn persisted_facts(messages: &[Message]) -> Vec<EvidenceFact> {
    messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| {
            if block.block_type != BlockType::ToolResult {
                return None;
            }
            let metadata = block.operation_metadata.as_ref()?;
            Some(EvidenceFact {
                tool_call_id: block.tool_call_id.clone(),
                tool_name: block.tool_name.clone(),
                disposition: metadata.disposition,
                effect_certainty: metadata.effect_certainty,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use otto_core::model::{Block, EffectCertainty, OperationDisposition, ToolResultMetadata};

    use super::*;
    use crate::tool::testutil::raw;

    fn result(id: &str, name: &str, text: &str, metadata: Option<ToolResultMetadata>) -> Message {
        Message {
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: id.into(),
                tool_name: name.into(),
                text: text.into(),
                operation_metadata: metadata,
                ..Block::default()
            }],
            ..Message::default()
        }
    }

    fn typed(
        disposition: OperationDisposition,
        effect_certainty: EffectCertainty,
    ) -> ToolResultMetadata {
        ToolResultMetadata {
            operation_id: None,
            disposition,
            effect_certainty,
            stop_reason: None,
        }
    }

    #[test]
    fn accepted_output_lists_canonical_typed_evidence() {
        let messages = vec![result(
            "call-7",
            "anything",
            "this output is deliberately not parsed",
            Some(typed(
                OperationDisposition::Succeeded,
                EffectCertainty::Completed,
            )),
        )];
        let result = submit(
            &raw(
                r#"{"claim":"works","verdict":"passed","evidence":[{"tool_call_id":"call-7","relation":"supports"}],"limitations":["narrow test"]}"#,
            ),
            &messages,
        );
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            concat!(
                r#"{"verdict":"passed","claim":"works","evidence":[{"tool_call_id":"call-7","#,
                r#""relation":"supports","tool_name":"anything","disposition":"succeeded","#,
                r#""effect_certainty":"completed"}],"limitations":["narrow test"]}"#,
            )
        );
    }

    #[test]
    fn untyped_result_is_not_authoritative_evidence() {
        let messages = vec![result("old", "read", "success-looking words", None)];
        let result = submit(
            &raw(
                r#"{"claim":"works","verdict":"passed","evidence":[{"tool_call_id":"old","relation":"supports"}],"limitations":[]}"#,
            ),
            &messages,
        );
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("not a prior persisted typed ToolResult")
        );
    }

    #[test]
    fn output_content_and_tool_name_do_not_change_consistency() {
        for (name, text) in [("bash", "FAILED"), ("made_up_tool", "all checks passed")] {
            let messages = vec![result(
                "same-id",
                name,
                text,
                Some(typed(
                    OperationDisposition::Succeeded,
                    EffectCertainty::Completed,
                )),
            )];
            let result = submit(
                &raw(
                    r#"{"claim":"works","verdict":"passed","evidence":[{"tool_call_id":"same-id","relation":"supports"}],"limitations":[]}"#,
                ),
                &messages,
            );
            assert!(!result.is_error, "name={name} text={text}: {result:?}");
        }
    }
}
