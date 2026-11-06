//! `otto eval memory-retraction`: the layer-2 and layer-3 evaluation of memory retraction
//! (issue #591 section 10). Layer 3 runs the real agent loop over synthetic transcripts under
//! four context conditions with an evaluation-only memory binding and measures whether the
//! answer still relies on an obsolete record; layer 2 runs the proposal generator over the
//! labeled dataset and scores the proposals. It calls a live provider through
//! `otto_core::agent::Agent` with the runtime's redactor; every text it writes passes that
//! redactor. It changes no session and registers no product tool or memory binding.

mod answer_checks;
mod argument_parsing;
mod conditions;
mod items;
mod judging;
mod layer_two;
mod memory_binding;
mod provider_capture;
mod report;
mod transcripts;

#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use otto_core::config::resolve::Runtime;
use otto_core::provider::Provider;
use tokio_util::sync::CancellationToken;

pub use argument_parsing::{Condition, Invocation, Layer, Options, Variant, parse};
pub use items::{ITEMS_PATH, ITEMS_VERSION, Item, Retirement, embedded_items};

use crate::eval_command::Roots;
use crate::eval_command::model_calls::EvalEnvironment;
use crate::eval_command::output_files::OutputDir;
use conditions::{AnswerRecord, Role};
use otto::runtime::builder::Builder;
use provider_capture::{BudgetedProvider, RequestLog};
use report::{ANSWERS_FILE, ITEMS_FILE, REPORT_JSON_FILE, REPORT_MARKDOWN_FILE};

/// The output directory of a run when `--out` is absent.
pub fn default_output_dir(home: &str, run_id: &str) -> PathBuf {
    PathBuf::from(home)
        .join(".otto/eval/memory-retraction")
        .join(run_id)
}

/// The run id of a new run: the UTC start time as `YYYYMMDDTHHMMSSZ`.
pub fn new_run_id() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Runs `--dry-run`: prints the item counts, the provider call estimate per layer and the
/// input byte estimate. It needs no credentials and makes no call.
pub fn run_dry(
    invocation: &Invocation,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    match argument_parsing::dry_run(&invocation.options, stdout) {
        Ok(()) => 0,
        Err(message) => crate::run::fail(stderr, &format!("eval memory-retraction: {message}")),
    }
}

/// Runs the evaluation against the runtime's provider.
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
            return crate::run::fail(stderr, &builder.redact_error(&message, Some(runtime)));
        }
    };
    let (provider, log) = BudgetedProvider::new(provider, invocation.options.max_calls);
    let env = Arc::new(EvalEnvironment {
        provider,
        redactor,
        provider_name: runtime.provider_type.clone(),
        profile_model: runtime.model.clone(),
        compressor_model: runtime.model.clone(),
        thinking: runtime.thinking.clone(),
        compaction: runtime.compaction,
        cancel: cancel.clone(),
    });
    match execute(&env, &log, invocation, roots, stdout).await {
        Ok(()) => 0,
        Err(message) => {
            let message = env.redactor.redact_string(&message);
            if cancel.is_cancelled() {
                let _ = writeln!(stderr, "otto: eval memory-retraction: {message}");
                return 130;
            }
            crate::run::fail(stderr, &format!("eval memory-retraction: {message}"))
        }
    }
}

/// Runs the selected layers over `env`. This is the provider-independent core the tests
/// drive: `env.provider` is the budgeted, request-logging provider and `log` its log.
pub async fn execute<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    log: &RequestLog,
    invocation: &Invocation,
    roots: &Roots,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    let options = &invocation.options;
    let run_id = new_run_id();
    let out_root = options
        .out
        .clone()
        .unwrap_or_else(|| default_output_dir(&roots.home, &run_id));
    let out = OutputDir::create(&out_root, env.redactor.clone())?;
    let items = items::selected_items(options.items.as_deref())?;
    out.write_json(ITEMS_FILE, &items)?;
    let started = chrono::Utc::now();
    let mut answers: Vec<AnswerRecord> = if out.exists(ANSWERS_FILE) {
        out.read_jsonl(ANSWERS_FILE)?
    } else {
        Vec::new()
    };
    if options.layer.includes(Layer::Three) {
        write_transcripts(&out, &items, &options.variants).await?;
        run_layer_three(env, log, options, &items, &out, &mut answers, stdout).await?;
    }
    let proposals = if options.layer.includes(Layer::Two) {
        layer_two::run_layer_two(env, &out, options.judge, stdout).await?
    } else {
        Vec::new()
    };
    let metadata = report::RunMetadata::collect(env, options, &run_id, started, log);
    let report = report::build_report(metadata, &items, &answers, &proposals);
    out.write_json(REPORT_JSON_FILE, &report)?;
    out.write_text(REPORT_MARKDOWN_FILE, &report::render_markdown(&report))?;
    writeln!(
        stdout,
        "report: {}",
        out.path(REPORT_MARKDOWN_FILE).display()
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Writes `transcripts/<item>-<variant>.jsonl` for every selected item and variant that has
/// no file yet: the constructed prior, one entry per line.
async fn write_transcripts(
    out: &OutputDir,
    items: &[Item],
    variants: &[Variant],
) -> Result<(), String> {
    let directory = out.path("transcripts");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    for item in items {
        for variant in variants {
            let name = transcripts::transcript_file(&item.name, *variant);
            if out.exists(&name) {
                continue;
            }
            let transcript = transcripts::build_transcript(item, *variant).await?;
            out.write_jsonl(&name, &transcript.entries)?;
        }
    }
    Ok(())
}

/// Runs every case of layer 3 that `answers` does not hold yet and appends each result to
/// `answers.jsonl` as it completes. The task answer is judged when `--no-judge` is absent.
async fn run_layer_three<P: Provider + Clone + Send + Sync + 'static>(
    env: &Arc<EvalEnvironment<P>>,
    log: &RequestLog,
    options: &Options,
    items: &[Item],
    out: &OutputDir,
    answers: &mut Vec<AnswerRecord>,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    for item in items {
        for variant in &options.variants {
            for condition in &options.conditions {
                if !conditions::applicable(*variant, *condition) {
                    continue;
                }
                for repetition in 0..options.repeat {
                    for role in [Role::Task, Role::Control] {
                        if env.cancel.is_cancelled() {
                            return Err("canceled".into());
                        }
                        let key = conditions::case_key(
                            &item.name, *variant, *condition, repetition, role,
                        );
                        if answers.iter().any(|answer| answer.key == key) {
                            continue;
                        }
                        let mut record = conditions::run_case(
                            env, log, item, *variant, *condition, repetition, role,
                        )
                        .await?;
                        if options.judge && role == Role::Task && record.error.is_empty() {
                            match judging::judge_task_answer(env, item, &record.answer).await {
                                Ok(verdict) => record.judge = Some(verdict),
                                Err(error) => record.judge_error = error,
                            }
                        }
                        out.append_jsonl(ANSWERS_FILE, &record)?;
                        writeln!(stdout, "{key}: {}", record.summary_line())
                            .map_err(|error| error.to_string())?;
                        answers.push(record);
                    }
                }
            }
        }
    }
    Ok(())
}
