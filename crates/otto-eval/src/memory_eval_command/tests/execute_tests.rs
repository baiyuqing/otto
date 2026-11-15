//! Tests of the orchestration: `execute` over a scripted provider, rerun reuse, judge
//! routing, redaction of every written file, and `run_dry` through the public entry point.

use std::path::Path;
use std::sync::Arc;

use otto_core::agent::redactor::{REDACTION_MARKER, Redactor};
use otto_core::config::resolve::CompactionRuntime;
use tokio_util::sync::CancellationToken;

use crate::eval_command::model_calls::EvalEnvironment;
use crate::eval_command::tests::scripted_provider::{
    ScriptedProvider, first_user_text, last_user_text, roots, text_reply,
};
use crate::memory_eval_command::conditions::{AnswerRecord, Role};
use crate::memory_eval_command::provider_capture::{BudgetedProvider, RequestLog, request_text};
use crate::memory_eval_command::{Invocation, execute, parse, run_dry};

const FIXTURE_SECRET: &str = "sk-test-eval-secret-0002";
const INSTALL_MARKER: &str = "m-30-npmi";

/// A rebuilt summary with the headings the compaction validator requires and no obsolete term.
const REBUILT_SUMMARY: &str = "## Goal\nInstall the dependencies of project alpha.\n\
## Constraints & Preferences\nNone.\n\
## Observations\nA statement about the install tool was withdrawn.\n\
## Progress\n### Done\nNone.\n### In Progress\nNone.\n### Blocked\nNone.\n\
## Key Decisions\nNone.\n\
## Next Steps\nAsk for the current install command.\n\
## Critical Context\nNone.";

/// Answers by request: the judge replies with a verdict, the summarizer with the rebuilt
/// summary, a request holding the marker with the obsolete command, the control task with
/// the yarn command, everything else with the pnpm command followed by `suffix`.
fn scripted(suffix: &'static str) -> ScriptedProvider {
    ScriptedProvider::new(move |request| {
        if request.system_prompt.contains("You grade one answer") {
            return text_reply(
                r#"{"relies_on_obsolete": false, "uses_replacement": true, "admits_uncertainty": false, "reason": "graded"}"#,
            );
        }
        if first_user_text(request).contains("<summary-mode>") {
            return text_reply(REBUILT_SUMMARY);
        }
        if request_text(request).contains(INSTALL_MARKER) {
            text_reply("Run npm install in the repository root.")
        } else if last_user_text(request).contains("project beta") {
            text_reply("Run yarn install in the repository root.")
        } else {
            text_reply(&format!("Run pnpm install in the repository root.{suffix}"))
        }
    })
}

type Environment = Arc<EvalEnvironment<BudgetedProvider<ScriptedProvider>>>;

fn environment(provider: ScriptedProvider, redactor: Redactor) -> (Environment, RequestLog) {
    let (provider, log) = BudgetedProvider::new(provider, 500);
    let env = Arc::new(EvalEnvironment {
        provider,
        redactor,
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
    (env, log)
}

fn invocation(arguments: &[&str]) -> Invocation {
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| (*argument).to_string())
        .collect();
    parse(&arguments).expect("valid arguments")
}

fn read_rows(out: &Path) -> Vec<AnswerRecord> {
    std::fs::read_to_string(out.join("answers.jsonl"))
        .expect("answers file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("answer row"))
        .collect()
}

#[tokio::test]
async fn layer_three_writes_rows_transcripts_and_report_and_a_rerun_makes_no_call() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("out");
    let provider = scripted("");
    let (env, log) = environment(provider.clone(), Redactor::new(&[]));
    let invocation = invocation(&[
        "--layer",
        "3",
        "--items",
        "install_command_pnpm",
        "--variants",
        "native_recall,assistant_paraphrase",
        "--conditions",
        "A,C",
        "--no-judge",
        "--out",
        out.to_str().unwrap(),
    ]);
    let mut stdout = Vec::new();
    execute(
        &env,
        &log,
        &invocation,
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .expect("execute");

    let rows = read_rows(&out);
    assert_eq!(rows.len(), 8, "2 variants x 2 conditions x 2 roles");
    assert_eq!(provider.requests().len(), 8, "one provider call per row");
    assert!(rows.iter().all(|row| row.judge.is_none()));
    let task_a = rows
        .iter()
        .find(|row| row.key == "install_command_pnpm/native_recall/A/0/task")
        .expect("A task row");
    assert!(task_a.requests.marker_in_request && task_a.rules.continued_reliance);
    let task_c = rows
        .iter()
        .find(|row| row.key == "install_command_pnpm/assistant_paraphrase/C/0/task")
        .expect("C task row");
    assert!(task_c.requests.notice_in_request && !task_c.requests.marker_in_request);
    assert_eq!(task_c.rules.uses_replacement, Some(true));

    assert!(out.join("items.json").is_file());
    assert!(
        out.join("transcripts/install_command_pnpm-native_recall.jsonl")
            .is_file()
    );
    assert!(
        out.join("transcripts/install_command_pnpm-assistant_paraphrase.jsonl")
            .is_file()
    );
    let report = std::fs::read_to_string(out.join("report.md")).expect("report");
    assert!(report.contains("## Layer 3"));
    assert!(report.contains("### A/traceable"));
    assert!(report.contains("### C/traceable"));
    assert!(report.contains("No layer 2 rows."));
    let stdout = String::from_utf8(stdout).unwrap();
    assert!(stdout.contains("install_command_pnpm/native_recall/A/0/task: reliance=true"));
    assert!(stdout.contains("report: "));

    let mut stdout = Vec::new();
    execute(
        &env,
        &log,
        &invocation,
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .expect("rerun");
    assert_eq!(provider.requests().len(), 8, "a rerun reuses every row");
    assert_eq!(read_rows(&out).len(), 8);
    assert!(out.join("report.json").is_file());
}

#[tokio::test]
async fn the_judge_runs_for_task_answers_only() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("out");
    let provider = scripted("");
    let (env, log) = environment(provider.clone(), Redactor::new(&[]));
    let invocation = invocation(&[
        "--layer",
        "3",
        "--items",
        "install_command_pnpm",
        "--variants",
        "native_recall",
        "--conditions",
        "C",
        "--out",
        out.to_str().unwrap(),
    ]);
    let mut stdout = Vec::new();
    execute(
        &env,
        &log,
        &invocation,
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .expect("execute");
    let rows = read_rows(&out);
    assert_eq!(rows.len(), 2);
    let task = rows.iter().find(|row| row.role == Role::Task).unwrap();
    let control = rows.iter().find(|row| row.role == Role::Control).unwrap();
    let verdict = task.judge.as_ref().expect("task verdict");
    assert_eq!(verdict.uses_replacement, Some(true));
    assert!(!verdict.relies_on_obsolete);
    assert!(control.judge.is_none() && control.judge_error.is_empty());
    let judge_requests = provider
        .requests()
        .iter()
        .filter(|request| request.system_prompt.contains("You grade one answer"))
        .count();
    assert_eq!(judge_requests, 1);
    assert_eq!(
        provider.requests().len(),
        3,
        "two answers and one judge call"
    );
    let report = std::fs::read_to_string(out.join("report.md")).expect("report");
    assert!(report.contains("| continued reliance, judge | 0/1 |"));
}

#[tokio::test]
async fn every_written_file_holds_no_secret() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("out");
    let provider = scripted(" token sk-test-eval-secret-0002 end");
    let (env, log) = environment(provider, Redactor::new(&[FIXTURE_SECRET.to_string()]));
    let invocation = invocation(&[
        "--layer",
        "3",
        "--items",
        "install_command_pnpm",
        "--variants",
        "native_recall",
        "--conditions",
        "C",
        "--no-judge",
        "--out",
        out.to_str().unwrap(),
    ]);
    let mut stdout = Vec::new();
    execute(
        &env,
        &log,
        &invocation,
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .expect("execute");
    let mut files = Vec::new();
    collect_files(&out, &mut files);
    assert!(files.len() >= 4, "{files:?}");
    let mut redacted_somewhere = false;
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains(FIXTURE_SECRET),
            "{} holds the secret",
            file.display()
        );
        redacted_somewhere |= text.contains(REDACTION_MARKER);
    }
    assert!(
        redacted_somewhere,
        "the answer with the secret was redacted, not dropped"
    );
    assert!(!String::from_utf8(stdout).unwrap().contains(FIXTURE_SECRET));
}

fn collect_files(directory: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

#[test]
fn run_dry_prints_the_estimate_through_the_public_entry_point() {
    let invocation = invocation(&[
        "--dry-run",
        "--layer",
        "3",
        "--items",
        "install_command_pnpm",
        "--conditions",
        "A,D",
        "--variants",
        "native_recall,summary_with_references",
        "--no-judge",
    ]);
    assert!(invocation.is_dry_run());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    assert_eq!(run_dry(&invocation, &mut stdout, &mut stderr), 0);
    let stdout = String::from_utf8(stdout).unwrap();
    assert!(stdout.contains("items: 1"), "{stdout}");
    assert!(stdout.contains("layer 3 cases: 3"), "{stdout}");
    assert!(
        stdout.contains("provider calls total (estimate): 7"),
        "{stdout}"
    );
    assert!(stderr.is_empty());
}
