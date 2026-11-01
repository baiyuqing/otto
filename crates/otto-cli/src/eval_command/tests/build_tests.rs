use std::sync::Arc;

use otto_core::agent::redactor::Redactor;
use otto_core::model::Role;
use otto_core::view_tree::NodeAddress;

use super::scripted_provider::{
    ScriptedProvider, ends_with_tool_result, environment, first_user_text, invocation,
    last_user_text, roots, text_reply, tool_reply, user_message_count,
};
use super::session_fixture::write_linear_session;
use crate::eval_command::model_calls::EvalEnvironment;
use crate::eval_command::tree_build::{CALLS_FILE, CallRecord, NodeRecord, TREE_FILE};
use crate::eval_command::{execute, output_files::OutputDir};

async fn run_build(
    env: &Arc<EvalEnvironment<ScriptedProvider>>,
    directory: &std::path::Path,
    session: &std::path::Path,
    extra: &[&str],
) {
    let out = directory.join("out");
    let mut arguments = vec![
        "view-tree",
        "build",
        "--session",
        session.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ];
    arguments.extend_from_slice(extra);
    let mut stdout = Vec::new();
    execute(env, &invocation(&arguments), &roots(directory), &mut stdout)
        .await
        .unwrap();
}

fn output(directory: &std::path::Path) -> OutputDir {
    OutputDir::create(&directory.join("out"), Redactor::new(&[])).unwrap()
}

fn long_first_message(index: usize) -> String {
    if index == 0 {
        "x".repeat(2000)
    } else {
        "ok".to_string()
    }
}

#[tokio::test]
async fn nodes_that_fit_skip_the_model() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 3, |index| format!("short {index}"));
    let provider = ScriptedProvider::never_called();
    run_build(
        &environment(provider.clone(), Redactor::new(&[])),
        directory.path(),
        &session,
        &[],
    )
    .await;

    assert!(provider.requests().is_empty());
    let nodes: Vec<NodeRecord> = output(directory.path()).read_jsonl(TREE_FILE).unwrap();
    assert_eq!(nodes.len(), 4, "three leaves and node 0+2");
    let joined = nodes
        .iter()
        .find(|node| (node.id, node.n) == (0, 2))
        .unwrap();
    assert_eq!(joined.text, "user: short 0\nassistant: short 1");
}

#[tokio::test]
async fn a_reply_over_the_limit_is_retried_and_the_shortest_is_kept() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 2, long_first_message);
    // Five attempts, all over 512 bytes; the fourth is the shortest.
    let lengths = [700, 600, 650, 560, 580];
    let provider = ScriptedProvider::new(move |request| {
        let first = first_user_text(request);
        if first.contains("Summarize message 0") {
            let attempt = user_message_count(request);
            return text_reply(
                &"abcde"
                    .chars()
                    .nth(attempt - 1)
                    .unwrap()
                    .to_string()
                    .repeat(lengths[attempt - 1]),
            );
        }
        assert!(
            first.contains("Merge the two summaries"),
            "unexpected task: {first}"
        );
        text_reply("merged summary")
    });
    run_build(
        &environment(provider.clone(), Redactor::new(&[])),
        directory.path(),
        &session,
        &["--concurrency", "1"],
    )
    .await;

    let out = output(directory.path());
    let nodes: Vec<NodeRecord> = out.read_jsonl(TREE_FILE).unwrap();
    let leaf = nodes
        .iter()
        .find(|node| (node.id, node.n) == (0, 1))
        .unwrap();
    assert_eq!(leaf.text, "d".repeat(560));
    let calls: Vec<CallRecord> = out.read_jsonl(CALLS_FILE).unwrap();
    let leaf_call = calls.iter().find(|call| call.kind == "leaf").unwrap();
    assert_eq!((leaf_call.attempts, leaf_call.over_limit_replies), (5, 5));
    assert_eq!(leaf_call.model, "compressor-model");
    assert_eq!(leaf_call.usage.input_tokens, 500);
    assert_eq!(leaf_call.usage.cached_tokens, 200);
    let merge_call = calls.iter().find(|call| call.kind == "merge").unwrap();
    assert_eq!(merge_call.attempts, 1);
    // The retry asks for a shorter text in the same conversation.
    let retry = provider
        .requests()
        .into_iter()
        .find(|request| user_message_count(request) == 2)
        .unwrap();
    assert!(last_user_text(&retry).contains("over the limit of 512 bytes"));
    assert!(last_user_text(&retry).contains("700 bytes"));
}

#[tokio::test]
async fn a_rerun_skips_built_nodes() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 2, long_first_message);
    let provider = ScriptedProvider::new(|_| text_reply("short summary"));
    run_build(
        &environment(provider.clone(), Redactor::new(&[])),
        directory.path(),
        &session,
        &[],
    )
    .await;
    let first_requests = provider.requests().len();
    assert!(first_requests >= 1);
    let tree_before =
        std::fs::read_to_string(directory.path().join("out").join(TREE_FILE)).unwrap();

    run_build(
        &environment(ScriptedProvider::never_called(), Redactor::new(&[])),
        directory.path(),
        &session,
        &[],
    )
    .await;

    let out = output(directory.path());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("out").join(TREE_FILE)).unwrap(),
        tree_before
    );
    let calls: Vec<CallRecord> = out.read_jsonl(CALLS_FILE).unwrap();
    assert_eq!(calls.len(), first_requests);
}

#[tokio::test]
async fn an_interrupted_build_resumes_with_the_nodes_it_has() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 4, |_| "y".repeat(600));
    let provider = ScriptedProvider::new(|_| text_reply("short summary"));
    run_build(
        &environment(provider.clone(), Redactor::new(&[])),
        directory.path(),
        &session,
        &[],
    )
    .await;
    let all_calls = provider.requests().len();
    // Keep only the first two leaves, as if the process stopped there.
    let out = output(directory.path());
    let nodes: Vec<NodeRecord> = out.read_jsonl(TREE_FILE).unwrap();
    let kept: Vec<NodeRecord> = nodes
        .into_iter()
        .filter(|node| node.n == 1 && node.id < 2)
        .collect();
    out.write_jsonl(TREE_FILE, &kept).unwrap();
    out.write_jsonl::<CallRecord>(CALLS_FILE, &[]).unwrap();

    let resumed = ScriptedProvider::new(|_| text_reply("short summary"));
    run_build(
        &environment(resumed.clone(), Redactor::new(&[])),
        directory.path(),
        &session,
        &[],
    )
    .await;

    assert_eq!(
        resumed.requests().len(),
        all_calls - 2,
        "the two kept leaves are not summarized again"
    );
    let nodes: Vec<NodeRecord> = out.read_jsonl(TREE_FILE).unwrap();
    assert_eq!(nodes.len(), 7);
    assert!(nodes.iter().any(|node| {
        NodeAddress::new(node.id, node.n).is_ok_and(|a| a == NodeAddress { id: 0, n: 4 })
    }));
}

#[tokio::test]
async fn the_compressor_sees_the_built_view_before_its_node() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 2, |_| "z".repeat(700));
    let provider = ScriptedProvider::new(|request| {
        let first = first_user_text(request);
        text_reply(&format!(
            "S[{}]",
            first
                .lines()
                .find(|line| line.starts_with("Summarize message") || line.starts_with("Merge"))
                .unwrap_or("?")
        ))
    });
    run_build(
        &environment(provider.clone(), Redactor::new(&[])),
        directory.path(),
        &session,
        &["--concurrency", "1"],
    )
    .await;

    let second_leaf = provider
        .requests()
        .into_iter()
        .find(|request| first_user_text(request).contains("Summarize message 1"))
        .unwrap();
    assert!(
        first_user_text(&second_leaf).contains("0+1|S[Summarize message 0"),
        "{}",
        first_user_text(&second_leaf)
    );
    let first_leaf = provider
        .requests()
        .into_iter()
        .find(|request| first_user_text(request).contains("Summarize message 0"))
        .unwrap();
    assert!(!first_user_text(&first_leaf).contains("0+1|"));
}

#[tokio::test]
async fn a_compressor_tool_call_gets_an_error_result_and_is_counted() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 2, long_first_message);
    let provider = ScriptedProvider::new(|request| {
        if ends_with_tool_result(request) {
            return text_reply("summary after the refused tool call");
        }
        if first_user_text(request).contains("Summarize message 0") {
            return tool_reply("zoom", r#"{"id":0,"n":1}"#);
        }
        text_reply("merged")
    });
    run_build(
        &environment(provider.clone(), Redactor::new(&[])),
        directory.path(),
        &session,
        &[],
    )
    .await;

    let calls: Vec<CallRecord> = output(directory.path()).read_jsonl(CALLS_FILE).unwrap();
    let leaf_call = calls.iter().find(|call| call.kind == "leaf").unwrap();
    assert_eq!(leaf_call.tool_call_violations, 1);
    assert_eq!(leaf_call.usage.provider_calls, 2);
    let answered = provider
        .requests()
        .into_iter()
        .find(ends_with_tool_result)
        .unwrap();
    let result = answered.messages.last().unwrap();
    assert_eq!(result.role, Role::Tool);
    assert!(
        result
            .blocks
            .iter()
            .any(|block| block.is_error && block.text.contains("not available"))
    );
    // The tool definitions are the ones answering turns carry.
    assert_eq!(
        answered
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["zoom", "date"]
    );
}

#[tokio::test]
async fn dry_run_counts_calls_without_a_provider() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 3, long_first_message);
    let provider = ScriptedProvider::never_called();
    let env = environment(provider, Redactor::new(&[]));
    let mut stdout = Vec::new();
    let arguments = [
        "view-tree",
        "build",
        "--session",
        session.to_str().unwrap(),
        "--dry-run",
    ];
    execute(
        &env,
        &invocation(&arguments),
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .unwrap();
    let printed = String::from_utf8(stdout).unwrap();
    assert!(printed.contains("log: 3 messages, 2004 bytes"), "{printed}");
    assert!(printed.contains("leaf model calls: 1"), "{printed}");
    assert!(
        printed.contains("merge model calls (estimate): 1"),
        "{printed}"
    );
    assert!(!directory.path().join("out").exists());
}
