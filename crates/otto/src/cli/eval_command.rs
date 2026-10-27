//! `otto eval view-tree`: a development evaluation of the summary-tree view of a session.
//!
//! The command reads a session file, builds a summary tree of its messages with a compressor
//! model, writes probe questions, answers them from the view and from the session's current
//! context, and reports the result with a computed cache-cost comparison. It calls a live
//! provider through `otto_core::agent::Agent` with the runtime's redactor; every text it writes
//! passes through that redactor. It changes no session and registers no product tool.

mod answer_judging;
mod argument_parsing;
mod cost_input;
mod eval_report;
mod model_calls;
mod node_compression;
mod output_files;
mod probe_answering;
mod probe_generation;
mod session_input;
mod tool_executors;
mod tree_build;
mod view_replay;

#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_core::config::resolve::Runtime;
use otto_core::provider::Provider;
use otto_core::view_tree::{LogMessage, SummaryTree, cache_cost_report};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub use argument_parsing::{Action, EvalOptions, Invocation, parse};
use model_calls::{EvalEnvironment, TokenUsage};
use output_files::OutputDir;
use probe_answering::AnsweringContext;
use probe_generation::{PROBES_FILE, Probe};
use tree_build::{BuildSettings, CALLS_FILE, CallRecord, NodeRecord, TREE_FILE};
use view_replay::{replay_view, view_from_pairs, view_pairs};

use crate::runtime::builder::Builder;

const LOG_FILE: &str = "log.jsonl";
const VIEW_FILE: &str = "view.json";
const BUILD_FILE: &str = "build.json";
const COST_FILE: &str = "cost.json";
const REPORT_JSON_FILE: &str = "report.json";
const REPORT_MARKDOWN_FILE: &str = "report.md";

/// Where the session files and the evaluation output live.
pub struct Roots {
    pub session_root: PathBuf,
    pub workspace: String,
    pub home: String,
}

/// Runs `--dry-run`: prints the size of the build. It needs no credentials and makes no call.
pub fn run_dry(
    invocation: &Invocation,
    roots: &Roots,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    match dry_run(&invocation.options, roots, stdout) {
        Ok(()) => 0,
        Err(message) => crate::cli::run::fail(stderr, &format!("eval view-tree: {message}")),
    }
}

fn dry_run(
    options: &EvalOptions,
    roots: &Roots,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    let reference = options.session.as_deref().ok_or("--session is required")?;
    let path =
        session_input::resolve_session_path(reference, &roots.session_root, &roots.workspace)?;
    let session = session_input::load_session(&path)?;
    let estimate = tree_build::estimate_build(&session.log)?;
    let line = format!(
        "log: {} messages, {} bytes\nleaf model calls: {}\nmerge model calls (estimate): {}\ntask input bytes (estimate): {}",
        estimate.log_messages,
        estimate.log_bytes,
        estimate.leaf_calls,
        estimate.merge_calls,
        estimate.task_input_bytes
    );
    writeln!(stdout, "{line}").map_err(|error| error.to_string())
}

/// Runs the action against the runtime's provider.
pub async fn run(
    invocation: &Invocation,
    builder: &Builder,
    runtime: &Runtime,
    roots: &Roots,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let (provider, redactor) = match builder.eval_provider_access(runtime) {
        Ok(access) => access,
        Err(message) => {
            return crate::cli::run::fail(stderr, &builder.redact_error(&message, Some(runtime)));
        }
    };
    let env = Arc::new(EvalEnvironment {
        provider,
        redactor,
        provider_name: runtime.provider.clone(),
        profile_model: runtime.model.clone(),
        compressor_model: runtime
            .subagent_models
            .first()
            .cloned()
            .unwrap_or_else(|| runtime.model.clone()),
        thinking: runtime.thinking.clone(),
        compaction: runtime.compaction,
        cancel: cancel.clone(),
    });
    match execute(&env, invocation, roots, stdout).await {
        Ok(()) => 0,
        Err(message) => {
            let message = env.redactor.redact_string(&message);
            if cancel.is_cancelled() {
                let _ = writeln!(stderr, "otto: eval view-tree: {message}");
                return 130;
            }
            crate::cli::run::fail(stderr, &format!("eval view-tree: {message}"))
        }
    }
}

/// Runs the action over `env`. This is the provider-independent core the tests drive.
pub async fn execute<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    invocation: &Invocation,
    roots: &Roots,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    if !env.redactor.allows_dynamic_content() {
        return Err("the redaction boundary is closed, so no session text can be sent".into());
    }
    let options = &invocation.options;
    let (out, session) = match invocation.action {
        Action::Build | Action::All => {
            if options.dry_run {
                return dry_run(options, roots, stdout);
            }
            let reference = options.session.as_deref().ok_or("--session is required")?;
            let path = session_input::resolve_session_path(
                reference,
                &roots.session_root,
                &roots.workspace,
            )?;
            let session = session_input::load_session(&path)?;
            let out = OutputDir::create(
                &output_path(options, roots, &session.session_id),
                env.redactor.clone(),
            )?;
            (out, Some(session))
        }
        Action::Probes | Action::Run => {
            let root = options.out.as_deref().ok_or("--out is required")?;
            (OutputDir::create(root, env.redactor.clone())?, None)
        }
    };
    match (invocation.action, session) {
        (Action::Build, Some(session)) => build(env, options, &session, &out, stdout).await,
        (Action::Probes, _) => probes(env, options, &out, false, stdout).await.map(|_| ()),
        (Action::Run, _) => run_answers(env, options, &out, stdout).await,
        (Action::All, Some(session)) => {
            build(env, options, &session, &out, stdout).await?;
            probes(env, options, &out, true, stdout).await?;
            run_answers(env, options, &out, stdout).await
        }
        _ => Err("internal error: build needs a session".into()),
    }
}

fn output_path(options: &EvalOptions, roots: &Roots, session_id: &str) -> PathBuf {
    match &options.out {
        Some(path) => path.clone(),
        None => Path::new(&roots.home)
            .join(".otto/eval/view-tree")
            .join(session_id),
    }
}

async fn build<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    options: &EvalOptions,
    session: &session_input::SessionInput,
    out: &OutputDir,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| env.compressor_model.clone());
    out.write_jsonl(LOG_FILE, &session.log)?;
    let settings = BuildSettings {
        model: model.clone(),
        concurrency: options.concurrency,
        session_id: session.session_id.clone(),
    };
    let outcome = tree_build::build_tree(env, &session.log, &settings, out).await?;
    let replay = replay_view(
        &session.log,
        &outcome.tree,
        options.view_low_kb * 1024,
        options.view_high_kb * 1024,
    );
    out.write_json(VIEW_FILE, &view_pairs(&replay.final_view))?;
    let calls: Vec<CallRecord> = out.read_jsonl(CALLS_FILE)?;
    let mut compressor_usage = TokenUsage::default();
    for call in &calls {
        compressor_usage.add(&call.usage);
    }
    out.write_json(
        BUILD_FILE,
        &json!({
            "session_id": session.session_id,
            "session_path": session.path.to_string_lossy(),
            "model": model,
            "view_low_kb": options.view_low_kb,
            "view_high_kb": options.view_high_kb,
            "concurrency": options.concurrency,
            "log_messages": session.log.len(),
            "nodes": {"total": outcome.tree.len(), "resumed": outcome.resumed_nodes, "without_model_call_this_run": outcome.ready_nodes},
            "calls": calls,
            "compressor_usage": compressor_usage,
            "over_limit_replies": calls.iter().map(|call| call.over_limit_replies).sum::<u32>(),
            "tool_call_violations": calls.iter().map(|call| call.tool_call_violations).sum::<u32>(),
            "shrink_events": replay.shrinks,
        }),
    )?;
    let (limits, compaction_note) = cost_input::compaction_limits(&env.compaction);
    let input = cost_input::cost_input(
        &session.log,
        replay.turn_views.clone(),
        limits,
        options.prices,
        &compressor_usage,
    );
    let cost = json!({
        "prices": input.prices,
        "tools_system_bytes": input.tools_system_bytes,
        "compaction": limits,
        "turns": input.turns.len(),
        "compressor_usage": compressor_usage,
        "report": cache_cost_report(&input),
        "assumptions": [
            format!("compaction: {compaction_note}"),
            "both layouts use the same tools and system prompt size (the evaluation's system prompt and tool definitions)",
            "the figures are computed from byte sizes, not measured from a provider",
        ],
    });
    out.write_json(COST_FILE, &cost)?;
    out.write_text(
        REPORT_MARKDOWN_FILE,
        &format!(
            "# View-tree evaluation\n\n{}",
            eval_report::render_cost_section(&cost)
        ),
    )?;
    writeln!(
        stdout,
        "built {} nodes for {} messages with {} model calls; output in {}",
        outcome.tree.len(),
        session.log.len(),
        calls.len(),
        out.root().display()
    )
    .map_err(|error| error.to_string())
}

/// What the stages after `build` read from the output directory.
struct BuiltArtifacts {
    log: Arc<Vec<LogMessage>>,
    tree: Arc<SummaryTree>,
    final_view: Vec<otto_core::view_tree::NodeAddress>,
    session_path: PathBuf,
    session_id: String,
}

fn load_artifacts(out: &OutputDir) -> Result<BuiltArtifacts, String> {
    if !out.exists(BUILD_FILE) {
        return Err(format!(
            "{} has no {BUILD_FILE}; run `eval view-tree build` first",
            out.root().display()
        ));
    }
    let log: Vec<LogMessage> = out.read_jsonl(LOG_FILE)?;
    let mut tree = SummaryTree::new();
    let mut records: Vec<NodeRecord> = out.read_jsonl(TREE_FILE)?;
    records.sort_by_key(|record| (record.n, record.id));
    for record in records {
        tree.insert(
            otto_core::view_tree::NodeAddress::new(record.id, record.n)?,
            record.text,
        )?;
    }
    let pairs: Vec<[u64; 2]> = out.read_json(VIEW_FILE)?;
    let build: serde_json::Value = out.read_json(BUILD_FILE)?;
    let text = |name: &str| {
        build
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    Ok(BuiltArtifacts {
        log: Arc::new(log),
        tree: Arc::new(tree),
        final_view: view_from_pairs(&pairs)?,
        session_path: PathBuf::from(text("session_path").ok_or("build.json has no session_path")?),
        session_id: text("session_id").ok_or("build.json has no session_id")?,
    })
}

async fn probes<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    options: &EvalOptions,
    out: &OutputDir,
    reuse_existing: bool,
    stdout: &mut (dyn Write + Send),
) -> Result<Vec<Probe>, String> {
    if reuse_existing {
        let existing: Vec<Probe> = out.read_jsonl(PROBES_FILE)?;
        if !existing.is_empty() {
            writeln!(
                stdout,
                "reusing {} probes from {}",
                existing.len(),
                out.path(PROBES_FILE).display()
            )
            .map_err(|error| error.to_string())?;
            return Ok(existing);
        }
    }
    let artifacts = load_artifacts(out)?;
    let probes = probe_generation::generate_probes(
        env,
        &artifacts.log,
        &artifacts.final_view,
        options.probe_count,
        out,
    )
    .await?;
    let kept = probes.iter().filter(|probe| !probe.dropped).count();
    writeln!(
        stdout,
        "wrote {} probes ({kept} kept) to {}",
        probes.len(),
        out.path(PROBES_FILE).display()
    )
    .map_err(|error| error.to_string())?;
    Ok(probes)
}

fn read_probe_file(path: &Path) -> Result<Vec<Probe>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read probes {}: {error}", path.display()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .map_err(|error| format!("{} line {}: {error}", path.display(), index + 1))
        })
        .collect()
}

async fn run_answers<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    options: &EvalOptions,
    out: &OutputDir,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    let artifacts = load_artifacts(out)?;
    let probes = match &options.probes_file {
        Some(path) => read_probe_file(path)?,
        None => read_probe_file(&out.path(PROBES_FILE))?,
    };
    let session = session_input::load_session(&artifacts.session_path)?;
    let context = AnsweringContext {
        tree: artifacts.tree,
        log: artifacts.log,
        final_view: artifacts.final_view,
        baseline_messages: session.baseline_messages,
        session_id: artifacts.session_id,
        max_steps: options.max_steps,
    };
    let results = probe_answering::run_probes(env, &context, &probes, out).await?;
    let cost = if out.exists(COST_FILE) {
        Some(out.read_json::<serde_json::Value>(COST_FILE)?)
    } else {
        None
    };
    let report = eval_report::build_report(&results, cost);
    out.write_json(REPORT_JSON_FILE, &report)?;
    out.write_text(REPORT_MARKDOWN_FILE, &eval_report::render_markdown(&report))?;
    for (index, criterion) in report.acceptance.iter().enumerate() {
        writeln!(
            stdout,
            "criterion {}: {:?} ({})",
            index + 1,
            criterion.check,
            criterion.detail
        )
        .map_err(|error| error.to_string())?;
    }
    writeln!(
        stdout,
        "report written to {}",
        out.path(REPORT_MARKDOWN_FILE).display()
    )
    .map_err(|error| error.to_string())
}
