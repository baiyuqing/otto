use super::{decode, event, start, stop};
use crate::claude::{
    output_budget::OutputBudget,
    protocol::build_request,
    stream::{StreamAssembler, StreamError},
    tool_input::{ToolInputError, ToolInputFailureKind},
};
use crate::config::ModelLimits;
use crate::provider::Request;
use serde_json::json;

fn block(index: u64, input: &str, explicit_stop: bool) -> String {
    let mut body = event(json!({"type":"content_block_start","index":index,
        "content_block":{"type":"tool_use","id":format!("c{index}"),"name":"lookup","input":{}}}))
        + &event(json!({"type":"content_block_delta","index":index,
            "delta":{"type":"input_json_delta","partial_json":input}}));
    if explicit_stop {
        body += &event(json!({"type":"content_block_stop","index":index}));
    }
    body
}
fn failure(body: &str) -> ToolInputError {
    match decode(body).unwrap_err() {
        StreamError::MalformedArguments(error) => error,
        error => panic!("unexpected error {error}"),
    }
}

#[test]
fn invalid_block_stop_retains_final_metadata_but_never_completes_successfully() {
    for explicit_stop in [false, true] {
        let prefix = start() + &block(0, "{\"private-value\":", explicit_stop);
        let mut assembler = StreamAssembler::new();
        assembler.push(prefix.as_bytes(), &mut |_| {}).unwrap();
        assert!(!assembler.is_done());
        assert_eq!(decode(&prefix), Err(StreamError::MissingStop));
        let ending = event(
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},
            "usage":{"output_tokens":4096}}),
        ) + &event(json!({"type":"message_stop"}));
        let error = assembler.push(ending.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(!assembler.is_done());
        let StreamError::MalformedArguments(error) = error else {
            panic!("expected tool error")
        };
        assert!(error.is_token_limit_truncation());
        assert_eq!(error.index, 0);
        assert_eq!(error.output_tokens, Some(4096));
        assert_eq!(error.stop_reason, "max_tokens");
        assert_eq!(error.failure.kind, ToolInputFailureKind::Incomplete);
        assert!(error.failure.line > 0);
        assert!(error.failure.column > 0);
        assert!(!error.to_string().contains("private-value"));
    }
}

#[test]
fn complete_invalid_inputs_are_not_classified_as_recoverable_truncation() {
    for (arguments, kind) in [
        ("{\"private-key\":]", ToolInputFailureKind::Syntax),
        (
            "{\"private-key\":1,\"private-key\":2}",
            ToolInputFailureKind::DuplicateKey,
        ),
        (
            "{\"x\":{\"y\":1,\"y\":2}}",
            ToolInputFailureKind::DuplicateKey,
        ),
        ("[]", ToolInputFailureKind::NonObject),
        ("null", ToolInputFailureKind::NonObject),
        ("1", ToolInputFailureKind::NonObject),
        ("{}{}", ToolInputFailureKind::Syntax),
    ] {
        for explicit_stop in [false, true] {
            let error =
                failure(&(start() + &block(0, arguments, explicit_stop) + &stop("max_tokens")));
            assert_eq!(error.failure.kind, kind);
            assert!(!error.is_token_limit_truncation());
            assert!(!error.to_string().contains("private-key"));
        }
    }
}

#[test]
fn incomplete_inputs_without_max_tokens_do_not_trigger_recovery() {
    for reason in [
        "end_turn",
        "tool_use",
        "model_context_window_exceeded",
        "private-future-reason",
    ] {
        let error = failure(&(start() + &block(0, "{", true) + &stop(reason)));
        assert_eq!(error.failure.kind, ToolInputFailureKind::Incomplete);
        assert!(!error.is_token_limit_truncation());
        assert!(!error.to_string().contains("private-future-reason"));
        if reason == "private-future-reason" {
            assert_eq!(error.stop_reason, "unknown");
        }
    }
}

#[test]
fn a_complete_invalid_block_disqualifies_recovery_for_the_entire_response() {
    for duplicate_index in [0, 1] {
        let inputs = if duplicate_index == 0 {
            ["{\"x\":1,\"x\":2}", "{"]
        } else {
            ["{", "{\"x\":1,\"x\":2}"]
        };
        let error = failure(
            &(start()
                + &block(0, inputs[0], true)
                + &block(1, inputs[1], false)
                + &stop("max_tokens")),
        );
        assert_eq!(error.index, duplicate_index);
        assert_eq!(error.failure.kind, ToolInputFailureKind::DuplicateKey);
        assert!(!error.is_token_limit_truncation());
    }
}

#[test]
fn closed_invalid_blocks_still_reject_further_mutation_and_duplicate_stop() {
    let prefix = start() + &block(0, "{", true);
    for invalid in [
        event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"}"}}),
        ),
        event(json!({"type":"content_block_stop","index":0})),
    ] {
        assert!(matches!(
            decode(&(prefix.clone() + &invalid)),
            Err(StreamError::Sequence(_))
        ));
    }
    assert_eq!(
        decode(&(prefix + &event(json!({"type":"error"})))),
        Err(StreamError::Api)
    );
}

#[test]
fn current_model_capacities_come_from_the_canonical_catalog_and_budgets_are_bounded() {
    for model in [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
    ] {
        let limits = ModelLimits::for_model(model);
        assert!(limits.known);
        assert_eq!(limits.context_window, 1_000_000);
        assert_eq!(limits.max_output_tokens, 128_000);
        let budget = OutputBudget::for_model(model);
        assert_eq!((budget.initial, budget.ceiling), (16_384, 128_000));
        let mut current = budget.initial;
        let mut budgets = vec![current];
        while let Some(next) = budget.grow(current) {
            assert!(next > current);
            current = next;
            budgets.push(current);
        }
        assert_eq!(budgets, [16_384, 32_768, 65_536, 128_000]);
        assert_eq!(
            build_request(&Request {
                model: model.into(),
                ..Request::default()
            })
            .unwrap()
            .max_tokens,
            16_384
        );
    }
    for (model, limit) in [
        ("claude-3-haiku", 4096),
        ("claude-3-5-sonnet", 8192),
        ("unknown-claude", 4096),
        ("gpt-5", 4096),
    ] {
        let budget = OutputBudget::for_model(model);
        assert_eq!((budget.initial, budget.ceiling), (limit, limit));
        assert_eq!(budget.grow(limit), None);
    }
}
