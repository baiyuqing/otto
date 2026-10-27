use std::sync::atomic::{AtomicUsize, Ordering};

use otto_core::agent::redactor::Redactor;
use otto_core::provider::Request;
use otto_core::view_tree::{NodeAddress, SummaryTree};
use serde_json::Value;

use super::scripted_provider::{
    ScriptedProvider, ends_with_tool_result, environment, first_user_text, invocation, roots,
    text_reply, tool_reply,
};
use super::session_fixture::{message_text, write_linear_session};
use crate::cli::eval_command::answer_judging::{chain_to_source, classify_failure};
use crate::cli::eval_command::probe_answering::{ANSWERS_FILE, ProbeResult};
use crate::cli::eval_command::probe_generation::{PROBES_FILE, Probe};
use crate::cli::eval_command::tool_executors::ToolCallRecord;
use crate::cli::eval_command::{execute, output_files::OutputDir};

/// Routes each request to the stage that sent it. The k-th generated question carries the mode
/// `modes[k % modes.len()]` in brackets: `open` is answered after opening the source message,
/// `skip` after zooming into a range only, `drop` is rejected by validation, and `invalid`
/// is generated as text that is not JSON.
pub fn pipeline_provider(modes: &'static [&'static str]) -> ScriptedProvider {
    let generated = AtomicUsize::new(0);
    ScriptedProvider::new(move |request| {
        let system = request.system_prompt.as_str();
        let first = first_user_text(request);
        if first.contains("Summarize message") || first.contains("Merge the two summaries") {
            return text_reply("compressed text");
        }
        if system.contains("You write one test question") {
            let source: u64 = first
                .lines()
                .next()
                .unwrap()
                .trim_start_matches("Target message id: ")
                .parse()
                .unwrap();
            let mode = modes[generated.fetch_add(1, Ordering::SeqCst) % modes.len()];
            if mode == "invalid" {
                return text_reply("I cannot write JSON");
            }
            return text_reply(&format!(
                r#"{{"question": "What does message {source} say? [{mode}]", "expected": "message number {source}"}}"#
            ));
        }
        if system.contains("You check a test question") {
            if first.contains("[drop]") {
                return text_reply(
                    r#"{"verdict": "drop", "reason": "the question names the answer"}"#,
                );
            }
            return text_reply(r#"{"verdict": "keep", "reason": "stated in the source"}"#);
        }
        if system.contains("You grade an answer") {
            let answer = first.split("Answer to grade: ").nth(1).unwrap_or("").trim();
            let wrong = answer.is_empty() || answer == "no idea";
            return text_reply(&format!(
                r#"{{"verdict": "{}", "reason": "graded"}}"#,
                if wrong { "wrong" } else { "correct" }
            ));
        }
        if system.contains("chain of nodes") {
            return text_reply(r#"{"mentions": false, "reason": "no summary names it"}"#);
        }
        if system.contains("Answer the question about the conversation above") {
            return text_reply("baseline answer");
        }
        view_answer(request, &first)
    })
}

pub const DEFAULT_MODES: &[&str] = &["open", "skip"];

fn view_answer(request: &Request, first: &str) -> otto_core::provider::Response {
    let question = first
        .lines()
        .find(|line| line.starts_with("Question: "))
        .unwrap();
    let source: u64 = question
        .trim_start_matches("Question: What does message ")
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let opens = question.contains("[open]");
    let steps = request
        .messages
        .iter()
        .filter(|message| message.role == otto_core::model::Role::Tool)
        .count();
    let range_start = source - source % 8;
    match steps {
        0 => tool_reply("zoom", &format!(r#"{{"id":{range_start},"n":8}}"#)),
        1 if opens => tool_reply("zoom", &format!(r#"{{"id":{source},"n":1}}"#)),
        _ if opens => text_reply(&format!("message number {source}")),
        _ => text_reply("no idea"),
    }
}

async fn run_all(
    directory: &std::path::Path,
    provider: ScriptedProvider,
    extra: &[&str],
) -> OutputDir {
    let session = write_linear_session(directory, 48, message_text);
    let out = directory.join("out");
    let mut arguments = vec![
        "view-tree",
        "all",
        "--session",
        session.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--view-low",
        "1",
        "--view-high",
        "1",
        "--count",
        "4",
    ];
    arguments.extend_from_slice(extra);
    let env = environment(provider, Redactor::new(&[]));
    let mut stdout = Vec::new();
    execute(
        &env,
        &invocation(&arguments),
        &roots(directory),
        &mut stdout,
    )
    .await
    .unwrap();
    OutputDir::create(&out, Redactor::new(&[])).unwrap()
}

#[tokio::test]
async fn zoom_calls_are_executed_and_recorded_and_source_flags_are_computed() {
    let directory = tempfile::tempdir().unwrap();
    let provider = pipeline_provider(DEFAULT_MODES);
    let out = run_all(directory.path(), provider.clone(), &[]).await;

    let results: Vec<ProbeResult> = out.read_jsonl(ANSWERS_FILE).unwrap();
    assert_eq!(results.len(), 4);
    for result in &results {
        let source = result.probe.sources[0];
        let range = format!(r#"{{"id":{},"n":8}}"#, source - source % 8);
        assert_eq!(result.zoom_calls[0].tool, "zoom");
        assert_eq!(result.zoom_calls[0].arguments, range);
        assert!(!result.zoom_calls[0].error);
        assert!(
            result.reached,
            "zoom of the range covering source {source} reached it"
        );
        if result.probe.question.ends_with("[open]") {
            assert_eq!(result.zoom_calls.len(), 2);
            assert_eq!(
                result.zoom_calls[1].arguments,
                format!(r#"{{"id":{source},"n":1}}"#)
            );
            assert!(result.opened_source);
            assert_eq!(result.view_verdict.verdict, "correct");
            assert_eq!(result.failure_class, "");
        } else {
            assert_eq!(result.zoom_calls.len(), 1);
            assert!(!result.opened_source);
            assert_eq!(result.view_verdict.verdict, "wrong");
            assert_eq!(result.failure_class, "missing");
        }
        assert_eq!(result.baseline_verdict.verdict, "correct");
        assert!(result.view_answer.usage.input_tokens > 0);
    }
    // The zoom tool returned the real children: the second request of a view answer holds them.
    let zoom_result = provider
        .requests()
        .into_iter()
        .find(|request| ends_with_tool_result(request) && request.system_prompt.contains("zoom"))
        .unwrap();
    let text = zoom_result
        .messages
        .last()
        .unwrap()
        .blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<String>();
    assert!(
        text.contains("compressed text") || text.contains("message number"),
        "{text}"
    );
    assert!(
        !zoom_result
            .messages
            .last()
            .unwrap()
            .blocks
            .iter()
            .any(|block| block.is_error)
    );
}

#[tokio::test]
async fn the_report_lists_probes_totals_criteria_and_the_cost_section() {
    let directory = tempfile::tempdir().unwrap();
    let out = run_all(directory.path(), pipeline_provider(DEFAULT_MODES), &[]).await;

    let report: Value = out.read_json("report.json").unwrap();
    assert_eq!(report["totals"]["probes"], 4);
    let wrong = report["totals"]["view"]["wrong"].as_u64().unwrap();
    assert_eq!(
        report["totals"]["failure_classes"]["missing"]
            .as_u64()
            .unwrap(),
        wrong
    );
    assert_eq!(report["acceptance"].as_array().unwrap().len(), 3);
    // Two of four view answers are wrong ([skip]) and every baseline answer is right: criterion 1 fails.
    assert_eq!(report["acceptance"][0]["check"], "fail");
    assert_eq!(report["acceptance"][1]["check"], "pass");
    assert!(
        report["cost"]["report"]["view_warm"]["per_turn"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    let markdown = std::fs::read_to_string(out.path("report.md")).unwrap();
    assert!(markdown.contains("## Acceptance criteria"));
    assert!(markdown.contains("computed from byte sizes"));
    assert!(markdown.contains("**fail**"));
    let kept: Vec<Probe> = out.read_jsonl(PROBES_FILE).unwrap();
    assert!(kept.iter().all(|probe| !probe.dropped));
}

#[tokio::test]
async fn a_rerun_of_all_reuses_probes_and_answers() {
    let directory = tempfile::tempdir().unwrap();
    run_all(directory.path(), pipeline_provider(DEFAULT_MODES), &[]).await;
    let session = directory
        .path()
        .join(format!("{}.jsonl", super::session_fixture::SESSION_ID));
    let out = directory.path().join("out");
    let env = environment(ScriptedProvider::never_called(), Redactor::new(&[]));
    let arguments = [
        "view-tree",
        "all",
        "--session",
        session.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--view-low",
        "1",
        "--view-high",
        "1",
    ];
    let mut stdout = Vec::new();
    execute(
        &env,
        &invocation(&arguments),
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .unwrap();
    assert!(
        String::from_utf8(stdout)
            .unwrap()
            .contains("reusing 4 probes")
    );
}

#[tokio::test]
async fn invalid_and_dropped_probes_are_kept_with_a_reason_and_skipped_by_run() {
    let directory = tempfile::tempdir().unwrap();
    let provider = pipeline_provider(&["open", "invalid", "drop"]);
    let out = run_all(directory.path(), provider, &["--count", "6"]).await;

    let probes: Vec<Probe> = out.read_jsonl(PROBES_FILE).unwrap();
    assert_eq!(probes.len(), 6);
    let dropped: Vec<&Probe> = probes.iter().filter(|probe| probe.dropped).collect();
    assert_eq!(dropped.len(), 4);
    let reasons: Vec<&str> = dropped
        .iter()
        .map(|probe| probe.reason.as_deref().unwrap())
        .collect();
    assert_eq!(
        reasons
            .iter()
            .filter(|reason| reason.contains("no JSON object"))
            .count(),
        2
    );
    assert_eq!(
        reasons
            .iter()
            .filter(|reason| reason.contains("the question names the answer"))
            .count(),
        2
    );
    let results: Vec<ProbeResult> = out.read_jsonl(ANSWERS_FILE).unwrap();
    assert_eq!(results.len(), 2, "run answers only the kept probes");
    assert!(results.iter().all(|result| !result.probe.dropped));
}

#[tokio::test]
async fn the_step_limit_stops_a_view_answer() {
    let directory = tempfile::tempdir().unwrap();
    let base = pipeline_provider(DEFAULT_MODES);
    let counter = std::sync::atomic::AtomicU64::new(0);
    let provider = ScriptedProvider::new(move |request| {
        if request
            .system_prompt
            .contains("You write one test question")
            || request.system_prompt.contains("You check")
            || request.system_prompt.contains("You grade")
            || request.system_prompt.contains("chain of nodes")
            || first_user_text(request).contains("Summarize message")
            || first_user_text(request).contains("Merge the two")
            || request.system_prompt.contains("Answer the question about")
        {
            return base.reply(request);
        }
        // A view answer that never stops zooming; each call differs so no repeat is detected.
        let next = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tool_reply("zoom", &format!(r#"{{"id":{},"n":1}}"#, next % 40))
    });
    let out = run_all(
        directory.path(),
        provider,
        &["--count", "2", "--max-steps", "3"],
    )
    .await;
    let results: Vec<ProbeResult> = out.read_jsonl(ANSWERS_FILE).unwrap();
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(result.view_answer.usage.provider_calls, 3);
        assert!(
            result
                .view_answer
                .error
                .contains("provider call limit of 3"),
            "{}",
            result.view_answer.error
        );
        assert_eq!(result.zoom_calls.len(), 3);
        assert_eq!(result.view_verdict.verdict, "wrong");
    }
}

fn tree_with_chain() -> (SummaryTree, Vec<NodeAddress>) {
    let mut tree = SummaryTree::new();
    for (id, n) in [(0, 1), (1, 1), (0, 2)] {
        tree.insert(
            NodeAddress::new(id, n).unwrap(),
            format!("text of {id}+{n}"),
        )
        .unwrap();
    }
    (tree, vec![NodeAddress::new(0, 2).unwrap()])
}

fn zoom_record(id: u64, n: u64) -> ToolCallRecord {
    ToolCallRecord {
        tool: "zoom".into(),
        arguments: format!(r#"{{"id":{id},"n":{n}}}"#),
        id: Some(id),
        n: Some(n),
        error: false,
    }
}

#[tokio::test]
async fn failure_classes_follow_the_node_texts_and_the_zoom_path() {
    let (tree, view) = tree_with_chain();
    assert_eq!(
        chain_to_source(&view, 1)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["0+2", "1+1"]
    );
    let probe = Probe {
        id: "p001".into(),
        question: "q".into(),
        expected: "e".into(),
        sources: vec![1],
        dropped: false,
        reason: None,
    };
    let classify = |mentions: bool, calls: Vec<ToolCallRecord>| {
        let view = view.clone();
        let tree = tree.clone();
        let probe = probe.clone();
        async move {
            let provider = ScriptedProvider::new(move |_| {
                text_reply(&format!(r#"{{"mentions": {mentions}, "reason": "r"}}"#))
            });
            let env = environment(provider.clone(), Redactor::new(&[]));
            let class = classify_failure(&env, &tree, &view, &probe, &calls)
                .await
                .unwrap();
            let request = provider.requests().remove(0);
            assert!(
                first_user_text(&request).contains("text of 0+2")
                    && first_user_text(&request).contains("text of 1+1")
            );
            class
        }
    };
    assert_eq!(classify(false, vec![zoom_record(0, 2)]).await, "missing");
    assert_eq!(classify(true, vec![zoom_record(0, 2)]).await, "not_zoomed");
    assert_eq!(
        classify(true, vec![zoom_record(0, 2), zoom_record(1, 1)]).await,
        "other"
    );
}
