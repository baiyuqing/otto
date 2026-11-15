//! Tests of the case runner through the real agent loop with a scripted provider.

use std::sync::{Arc, Mutex};

use otto_core::agent::redactor::Redactor;
use otto_core::config::resolve::CompactionRuntime;
use tokio_util::sync::CancellationToken;

use crate::eval_command::model_calls::EvalEnvironment;
use crate::eval_command::tests::scripted_provider::{
    ScriptedProvider, first_user_text, last_user_text, text_reply,
};
use crate::memory_eval_command::argument_parsing::{Condition, Variant};
use crate::memory_eval_command::conditions::{AnswerRecord, Role, run_case};
use crate::memory_eval_command::items::{Item, embedded_items};
use crate::memory_eval_command::provider_capture::{BudgetedProvider, RequestLog, request_text};

/// A summary with the ten headings the compaction validator requires. It holds no obsolete term.
const REBUILT_SUMMARY: &str = "## Goal\nInstall the dependencies of project alpha.\n\
## Constraints & Preferences\nNone.\n\
## Observations\nA statement about the install tool was withdrawn.\n\
## Progress\n### Done\nNone.\n### In Progress\nNone.\n### Blocked\nNone.\n\
## Key Decisions\nNone.\n\
## Next Steps\nAsk for the current install command.\n\
## Critical Context\nNone.";

type CaseEnvironment = Arc<EvalEnvironment<BudgetedProvider<ScriptedProvider>>>;

/// The answers: the obsolete command when the request holds the marker, the yarn command for
/// the control task, the pnpm command otherwise; summary requests get `REBUILT_SUMMARY` and are
/// kept in `summary_requests`.
fn scripted(summary_requests: Arc<Mutex<Vec<String>>>) -> ScriptedProvider {
    ScriptedProvider::new(move |request| {
        if first_user_text(request).contains("<summary-mode>") {
            summary_requests.lock().unwrap().push(request_text(request));
            return text_reply(REBUILT_SUMMARY);
        }
        if request_text(request).contains("m-30-npmi") {
            text_reply("Run npm install in the repository root.")
        } else if last_user_text(request).contains("project beta") {
            text_reply("Run yarn install in the repository root.")
        } else {
            text_reply("Run pnpm install in the repository root.")
        }
    })
}

fn environment(max_calls: u32) -> (CaseEnvironment, RequestLog, Arc<Mutex<Vec<String>>>) {
    let summary_requests = Arc::new(Mutex::new(Vec::new()));
    let (provider, log) = BudgetedProvider::new(scripted(Arc::clone(&summary_requests)), max_calls);
    let env = Arc::new(EvalEnvironment {
        provider,
        redactor: Redactor::new(&[]),
        provider_name: "openai-compatible".into(),
        profile_model: "profile-model".into(),
        thinking: String::new(),
        compaction: CompactionRuntime {
            auto: false,
            context_window: 100_000,
            hard_input_window: 90_000,
            working_window: 80_000,
            max_output_tokens: 4_000,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
        },
        cancel: CancellationToken::new(),
    });
    (env, log, summary_requests)
}

fn install_item() -> Item {
    embedded_items().expect("items").remove(0)
}

async fn case(variant: Variant, condition: Condition, role: Role) -> (AnswerRecord, Vec<String>) {
    let (env, log, summary_requests) = environment(100);
    let record = run_case(&env, &log, &install_item(), variant, condition, 0, role)
        .await
        .expect("case runs");
    let summaries = summary_requests.lock().unwrap().clone();
    (record, summaries)
}

#[tokio::test]
async fn condition_a_sends_the_obsolete_record_and_the_answer_relies_on_it() {
    for variant in Variant::ALL {
        let (record, _) = case(variant, Condition::A, Role::Task).await;
        assert_eq!(record.error, "", "{variant:?}");
        assert!(record.requests.marker_in_request, "{variant:?}");
        assert!(!record.requests.notice_in_request, "{variant:?}");
        assert!(
            record.rules.continued_reliance,
            "{variant:?}: {}",
            record.answer
        );
        assert_eq!(record.rules.uses_replacement, Some(false));
        assert_eq!(record.traceable, variant.traceable());
        assert_eq!(
            record.key,
            format!("install_command_pnpm/{}/A/0/task", variant.as_str())
        );
        assert!(record.compaction.is_none());
        assert_eq!(record.usage.provider_calls, 1);
        assert_eq!(record.requests.request_count, 1);
    }
}

#[tokio::test]
async fn condition_b_drops_the_record_and_announces_nothing() {
    let (record, _) = case(Variant::AssistantParaphrase, Condition::B, Role::Task).await;
    assert!(!record.requests.marker_in_request);
    assert!(!record.requests.notice_in_request);
    assert!(record.requests.replacement_in_request);
    assert!(!record.rules.continued_reliance, "{}", record.answer);
    assert_eq!(record.rules.uses_replacement, Some(true));
}

#[tokio::test]
async fn condition_c_announces_the_retirement_for_traceable_copies() {
    for variant in [Variant::AssistantParaphrase, Variant::NativeRecall] {
        let (record, _) = case(variant, Condition::C, Role::Task).await;
        assert!(record.requests.notice_in_request, "{variant:?}");
        assert!(!record.requests.marker_in_request, "{variant:?}");
        assert!(record.requests.replacement_in_request, "{variant:?}");
        assert!(
            !record.rules.continued_reliance,
            "{variant:?}: {}",
            record.answer
        );
    }
}

#[tokio::test]
async fn condition_c_cannot_announce_for_untraceable_copies() {
    for variant in [Variant::ToolResult, Variant::LegacySummary] {
        let (record, _) = case(variant, Condition::C, Role::Task).await;
        assert!(!record.requests.notice_in_request, "{variant:?}");
        assert!(!record.requests.summary_affected_in_request, "{variant:?}");
        assert!(!record.requests.marker_in_request, "{variant:?}");
        assert!(record.requests.replacement_in_request, "{variant:?}");
    }
}

#[tokio::test]
async fn condition_c_marks_a_traced_summary_as_affected() {
    let (record, _) = case(Variant::SummaryWithReferences, Condition::C, Role::Task).await;
    assert!(record.requests.notice_in_request);
    assert!(record.requests.summary_affected_in_request);
}

#[tokio::test]
async fn condition_d_rebuilds_the_traced_summary_with_the_retractions() {
    let (record, summaries) = case(Variant::SummaryWithReferences, Condition::D, Role::Task).await;
    assert_eq!(record.error, "");
    let compaction = record.compaction.expect("compaction record");
    assert!(!compaction.noop);
    assert!(
        compaction.summary.starts_with(REBUILT_SUMMARY),
        "{}",
        compaction.summary
    );
    assert!(!compaction.summary_holds_obsolete_term);
    assert_eq!(summaries.len(), 1);
    assert!(
        summaries[0].contains("Retracted memory records:"),
        "{}",
        summaries[0]
    );
    assert!(summaries[0].contains("mem-install-npm"), "{}", summaries[0]);
    assert!(!record.requests.summary_affected_in_request);
    assert!(!record.requests.marker_in_request);
    assert_eq!(record.requests.request_count, 2);
    assert_eq!(record.usage.provider_calls, 2);
}

#[tokio::test]
async fn condition_d_rebuilds_the_untraced_summary_without_a_retraction_block() {
    let (record, summaries) = case(Variant::LegacySummary, Condition::D, Role::Task).await;
    assert_eq!(record.error, "");
    let compaction = record.compaction.expect("compaction record");
    assert!(!compaction.noop);
    assert!(
        compaction.summary.starts_with(REBUILT_SUMMARY),
        "{}",
        compaction.summary
    );
    assert_eq!(summaries.len(), 1);
    assert!(
        !summaries[0].contains("Retracted memory records:"),
        "{}",
        summaries[0]
    );
}

#[tokio::test]
async fn control_turn_fills_the_control_rules() {
    let (record, _) = case(Variant::AssistantParaphrase, Condition::C, Role::Control).await;
    assert_eq!(record.role, Role::Control);
    assert_eq!(
        record.rules.control_utility,
        Some(true),
        "{}",
        record.answer
    );
    assert_eq!(record.rules.control_collateral, Some(false));
    assert_eq!(record.rules.control_abstention, Some(false));
    assert!(!record.rules.continued_reliance);
    assert_eq!(record.rules.uses_replacement, None);
    assert!(record.key.ends_with("/control"));
}

#[tokio::test]
async fn provider_call_limit_lands_in_the_record_error() {
    let (env, log, _) = environment(1);
    let item = install_item();
    let first = run_case(
        &env,
        &log,
        &item,
        Variant::NativeRecall,
        Condition::A,
        0,
        Role::Task,
    )
    .await
    .expect("first case");
    assert_eq!(first.error, "");
    let second = run_case(
        &env,
        &log,
        &item,
        Variant::NativeRecall,
        Condition::A,
        1,
        Role::Task,
    )
    .await
    .expect("a refused call is a record, not an Err");
    assert!(
        second.error.contains("provider call limit"),
        "{:?}",
        second.error
    );
    assert_eq!(second.answer, "");
}

#[tokio::test]
async fn compaction_failure_is_recorded_and_the_turn_still_runs() {
    let (env, log, _) = environment(1);
    // The one allowed call goes to the compaction summary; the turn call is refused.
    let record = run_case(
        &env,
        &log,
        &install_item(),
        Variant::SummaryWithReferences,
        Condition::D,
        0,
        Role::Task,
    )
    .await
    .expect("case runs");
    assert!(record.compaction.is_some());
    assert!(
        record.error.contains("provider call limit"),
        "{:?}",
        record.error
    );
    assert_eq!(record.answer, "");
}
