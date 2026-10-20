//! Session reflection: look back at finished work and propose what is worth
//! remembering.
//!
//! One run reads the part of a session no earlier run covered, asks the model
//! once, with no tools, what durable facts and preferences it contains,
//! checks the answer in code, and queues the survivors as memory candidates
//! for a human to review. It never writes a memory record directly, and it
//! never writes to session history. When the slice is clean it may also write
//! skills, after the vetting pipeline, into `~/.otto/skills`; every write is
//! reported and can be undone with `/skill revert`.
//!
//! Design: `docs/specs/2026-10-02-session-reflection.md`.
//!
//! Layout: [`transcript`] reads and classifies entries, [`taint`] decides
//! which are external, [`prompt`] builds the request, [`output`] parses and
//! validates the answer, [`evidence`] verifies its quotes, and [`store`] keeps
//! run rows, per-session watermarks and skill ownership. A skill passes
//! [`guard`] (structure and the rule scan) and [`review`] (a fail-closed model
//! review) before [`skillwrite`] may write it. This module composes them.
//!
//! Ownership: a [`Reflector`] is built once at the composition root and holds
//! the optional store and the resolved configuration. A run borrows the
//! runner, the session path, and the memory service for its duration.
//!
//! Concurrency and cancellation: a run does not lock; the caller serializes it
//! with turns and compactions (see `Controller::reflect`). `cancel` stops the
//! model call, records the run as canceled, and leaves the watermark in place.
//!
//! Errors: see [`Error`]. A failed or canceled run is recorded and can be
//! retried; the next run covers the same slice.

pub mod evidence;
pub mod guard;
pub mod migration;
pub mod output;
pub mod prompt;
pub mod repository;
pub mod review;
pub mod runlog;
pub mod skillwrite;
pub mod store;
pub mod taint;
pub mod transcript;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use otto_core::agent::oneshot::TextRequest;
use otto_core::config::{Auto, ReflectionRuntime, SkillSource};
use tokio_util::sync::CancellationToken;

use crate::memory::{
    CandidateAction, CandidateState, Origin, ProposeRequest, Provenance, Scope, SearchRequest,
    Service,
};
use crate::runtime::builder::Runner;
use output::{Action, Existing, Plan, Proposal, ScopeChoice};
use repository::ReflectionStore;
use store::RunRow;
pub use store::{Status, Store};
use transcript::EntryRole;

/// The largest accepted model answer, in bytes.
const MAXIMUM_ANSWER_BYTES: usize = 256 * 1024;
/// The metadata key carrying the reflection run id on a candidate.
pub const RUN_METADATA_KEY: &str = "reflection_run";
/// How many existing records and candidates one run reads per scope.
const CONTEXT_LIMIT: usize = 100;

#[derive(Debug)]
pub enum Error {
    Disabled,
    NoSession,
    MemoryUnavailable,
    /// The redaction boundary is closed, so transcript text cannot be sent.
    BoundaryClosed,
    Read(String),
    Model(String),
    Cancelled,
    InvalidAnswer(String),
    Store(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("reflection is disabled ([reflection].enabled)"),
            Self::NoSession => {
                formatter.write_str("reflection needs a persisted session (not --no-session)")
            }
            Self::MemoryUnavailable => formatter.write_str("memory is not available"),
            Self::BoundaryClosed => formatter
                .write_str("transcript text cannot be sent: the redaction boundary is closed"),
            Self::Read(message) => write!(formatter, "read session: {message}"),
            Self::Model(message) => write!(formatter, "reflection model call failed: {message}"),
            Self::Cancelled => formatter.write_str("reflection canceled"),
            Self::InvalidAnswer(message) => formatter.write_str(message),
            Self::Store(message) => write!(formatter, "record reflection run: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub run_id: String,
    pub status: Status,
    /// Candidate ids queued for review.
    pub candidates: Vec<String>,
    /// Proposals dropped, by reason, from validation and from the store.
    pub dropped: BTreeMap<&'static str, usize>,
    /// Skills written, with what was done to each.
    pub skills: Vec<(String, skillwrite::Written)>,
    pub skill_approvals: Vec<String>,
    /// Whether skill output was withheld because the slice held external
    /// content (`[reflection].skill_source = "untainted"`).
    pub skills_withheld: bool,
    pub entries: usize,
    /// Whether the slice contained external entries, which were withheld.
    pub tainted: bool,
    /// Whether the oldest entries were dropped to fit `max_input_bytes`.
    pub truncated: bool,
    /// Why the run was skipped, for a skipped run; empty otherwise.
    pub note: String,
    /// Every proposal the model returned, with its outcome: memories first,
    /// then skills, each in model order, then the proposals dropped by
    /// validation.
    pub items: Vec<runlog::Item>,
    /// The external entries of the slice as (id, tool name or role).
    pub external: Vec<(String, String)>,
    /// Why skills were not requested from the model; empty when they were.
    pub skills_note: &'static str,
    /// The run log: `None` when no log directory is configured or the run
    /// was skipped.
    pub log: Option<Result<PathBuf, String>>,
    /// The model's retrospective of the slice, redacted, with a field that
    /// still matched a secret rule replaced by a marker. Empty when the run
    /// made no model call or the model gave none.
    pub retrospective: String,
}

impl Report {
    /// A report for a run that did not start, with the reason in `note`.
    fn skipped(note: &str) -> Self {
        Self {
            run_id: String::new(),
            status: Status::Noop,
            candidates: Vec::new(),
            dropped: BTreeMap::new(),
            skills: Vec::new(),
            skill_approvals: Vec::new(),
            skills_withheld: false,
            entries: 0,
            tainted: false,
            truncated: false,
            note: note.to_owned(),
            items: Vec::new(),
            external: Vec::new(),
            skills_note: "",
            log: None,
            retrospective: String::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn skipped_for_test() -> Self {
        Self::skipped("test")
    }

    /// Whether the run did not start (`too_soon`, `min_turns`).
    pub fn is_skipped(&self) -> bool {
        self.status == Status::Noop && !self.note.is_empty()
    }

    /// What `/reflect` shows: [`Self::line`], then the retrospective when
    /// the run has one.
    pub fn summary(&self) -> String {
        summary_text(&self.line(), &self.retrospective)
    }

    /// One line for a frontend.
    pub fn line(&self) -> String {
        let dropped: usize = self.dropped.values().sum();
        let mut line = match self.status {
            Status::Noop if !self.note.is_empty() => format!("reflection: skipped ({})", self.note),
            Status::Noop => "reflection: nothing new to reflect on".to_owned(),
            _ => {
                let mut line = format!(
                    "reflection: {} candidate(s) queued for review (/memory review), {} dropped",
                    self.candidates.len(),
                    dropped
                );
                if !self.skill_approvals.is_empty() {
                    line.push_str(&format!(
                        "; {} skill approval(s) pending",
                        self.skill_approvals.len()
                    ));
                }
                for (name, written) in &self.skills {
                    let verb = match written {
                        skillwrite::Written::Created => "created",
                        skillwrite::Written::Revised => "revised",
                    };
                    line.push_str(&format!(
                        "; {verb} skill {name} (active in new sessions; undo with /skill revert {name})"
                    ));
                }
                if self.tainted {
                    line.push_str("; external content was withheld");
                }
                if self.skills_withheld {
                    line.push_str("; skills were not written because of it");
                }
                if self.truncated {
                    line.push_str("; the oldest entries were cut to fit");
                }
                line
            }
        };
        match &self.log {
            Some(Ok(path)) => line.push_str(&format!("; log: {}", path.display())),
            Some(Err(error)) => line.push_str(&format!("; run log not written: {error}")),
            None => {}
        }
        line
    }
}

/// [`Report::summary`] from its parts, for a client that has the report's
/// line and retrospective but not the report.
pub fn summary_text(line: &str, retrospective: &str) -> String {
    if retrospective.is_empty() {
        return line.to_owned();
    }
    format!("{line}\n\nRetrospective:\n{retrospective}")
}

/// What started a run. `Manual` (`/reflect`) and `Requested` (the model's
/// `reflect` tool, called when the user asks) ignore `[reflection].auto` and
/// the automatic cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Manual,
    Requested,
    OnWork,
    OnCompaction,
    OnExit,
}

impl Trigger {
    /// The name recorded in `reflection.db`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Requested => "requested",
            Self::OnWork => "on_work",
            Self::OnCompaction => "on_compaction",
            Self::OnExit => "on_exit",
        }
    }
}

/// The fewest seconds between two automatic runs for one session, counted
/// from when the earlier one started. A failed run counts: automatic runs
/// never retry.
pub const AUTO_MINIMUM_INTERVAL_SECONDS: i64 = 600;

/// What a run reads and writes besides the model call.
pub struct Context<'a> {
    pub approvals: Option<&'a Arc<crate::approval::Service>>,
    pub runner: &'a Runner,
    pub session_id: &'a str,
    pub presentation_origin: Option<&'a crate::schedule::Origin>,
    /// The session file; empty for a `--no-session` session.
    pub session_path: &'a str,
    pub service: &'a Arc<Service>,
    pub user_scope: &'a Scope,
    pub workspace_scope: &'a Scope,
    /// Where skills are written and looked up; `None` when skills are
    /// disabled (`[skills].enabled = false`) or there is no home directory.
    pub skill_roots: Option<&'a skillwrite::Roots>,
}

mod gate;

/// The reflection use case, built at the composition root.
pub struct Reflector {
    config: ReflectionRuntime,
    gate: Option<gate::Gate>,
    store: Option<Arc<dyn ReflectionStore>>,
    minimum_interval: chrono::Duration,
    /// Where each run writes its Markdown log; `None` writes none.
    log_dir: Option<PathBuf>,
}

impl Reflector {
    pub fn new(config: ReflectionRuntime, store: Option<Arc<Store>>) -> Self {
        Self::with_store(config, store.map(|store| store as Arc<dyn ReflectionStore>))
    }

    /// Builds a reflector with a selected persistence adapter.
    pub fn with_store(config: ReflectionRuntime, store: Option<Arc<dyn ReflectionStore>>) -> Self {
        Self {
            config,
            store,
            gate: None,
            minimum_interval: chrono::Duration::seconds(AUTO_MINIMUM_INTERVAL_SECONDS),
            log_dir: None,
        }
    }

    /// Adds the auxiliary automatic-reflection decision backend. Manual and
    /// requested reflection bypass it; it has no approval authority.
    pub fn with_decision_backend(
        mut self,
        backend: Arc<dyn crate::decision::Backend>,
        logger: Arc<crate::log::Logger>,
    ) -> Self {
        self.gate = Some(gate::Gate::new(backend, logger));
        self
    }

    /// Sets the directory the run logs are written to.
    pub fn with_log_dir(mut self, dir: PathBuf) -> Self {
        self.log_dir = Some(dir);
        self
    }

    /// The directory the run logs are written to, if any.
    pub fn log_dir(&self) -> Option<&Path> {
        self.log_dir.as_deref()
    }

    /// Replaces the minimum interval between automatic runs.
    #[cfg(test)]
    fn with_minimum_interval(mut self, interval: chrono::Duration) -> Self {
        self.minimum_interval = interval;
        self
    }

    /// When reflection runs by itself, or `Auto::Off` when it is disabled.
    pub fn auto(&self) -> Auto {
        if self.config.enabled {
            self.config.auto
        } else {
            Auto::Off
        }
    }

    /// Whether an automatic run for `session_id` started too recently to
    /// start another. A store that cannot be read blocks the run: an
    /// automatic run that cannot rely on its own limits does not start.
    pub fn too_soon(&self, session_id: &str) -> bool {
        let Some(store) = &self.store else {
            return true;
        };
        match store.last_auto_started(session_id) {
            Ok(None) => false,
            Ok(Some(started)) => chrono::DateTime::parse_from_rfc3339(&started)
                .map(|started| Utc::now() - started.with_timezone(&Utc) < self.minimum_interval)
                .unwrap_or(true),
            Err(_) => true,
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// The skills reflection owns, by name.
    pub fn generated_skills(&self) -> Result<Vec<store::GeneratedSkill>, Error> {
        let store = self.store.as_ref().ok_or(Error::Disabled)?;
        store
            .list_generated()
            .map_err(|error| Error::Store(error.to_string()))
    }

    /// Restores the previous version of a skill reflection generated, or
    /// removes it if reflection created it.
    pub fn revert_skill(
        &self,
        roots: &skillwrite::Roots,
        name: &str,
    ) -> Result<skillwrite::Reverted, skillwrite::RevertError> {
        let store = self.store.as_ref().ok_or_else(|| {
            skillwrite::RevertError::Failed("reflection is disabled ([reflection].enabled)".into())
        })?;
        skillwrite::revert(store.as_ref(), roots, name, &now())
    }

    /// The skill reflection owns under `name`, if any.
    pub fn generated_skill(&self, name: &str) -> Option<store::GeneratedSkill> {
        self.store.as_ref()?.generated(name).ok().flatten()
    }

    /// The newest run logs, newest first, for `/reflect list`. Empty when no
    /// log directory is configured.
    pub fn runs(&self, limit: usize) -> Result<Vec<runlog::Summary>, Error> {
        if !self.config.enabled {
            return Err(Error::Disabled);
        }
        match &self.log_dir {
            Some(dir) => runlog::list(dir, limit).map_err(Error::Read),
            None => Ok(Vec::new()),
        }
    }

    /// The run log `id` names, with its content, for `/reflect show`.
    pub fn run_log(&self, id: &str) -> Result<Option<(PathBuf, String)>, Error> {
        if !self.config.enabled {
            return Err(Error::Disabled);
        }
        match &self.log_dir {
            Some(dir) => runlog::read(dir, id).map_err(Error::Read),
            None => Ok(None),
        }
    }

    /// Runs one reflection over what no earlier run covered.
    pub async fn run(
        &self,
        context: &Context<'_>,
        trigger: Trigger,
        focus: &str,
        cancel: &CancellationToken,
    ) -> Result<Report, Error> {
        if !self.config.enabled {
            return Err(Error::Disabled);
        }
        let enabled = match trigger {
            Trigger::Manual | Trigger::Requested => true,
            Trigger::OnWork => self.auto() == Auto::OnWork,
            Trigger::OnCompaction => {
                matches!(self.auto(), Auto::OnCompaction | Auto::OnWork)
            }
            Trigger::OnExit => self.auto() == Auto::OnExit,
        };
        if !enabled {
            return Err(Error::Disabled);
        }
        if matches!(trigger, Trigger::OnWork | Trigger::OnCompaction)
            && self.too_soon(context.session_id)
        {
            return Ok(Report::skipped("an automatic run started recently"));
        }
        if context.session_path.is_empty() {
            return Err(Error::NoSession);
        }
        if !context.runner.allows_dynamic_content() {
            return Err(Error::BoundaryClosed);
        }
        let run_id = crate::memory::new_id().map_err(|_| Error::Store("run id".into()))?;
        let started_at = now();
        let session_id = context.session_id;
        let watermark = match &self.store {
            Some(store) => store
                .watermark(session_id)
                .map_err(|e| Error::Store(e.to_string()))?,
            None => None,
        };
        let redact = |text: &str| context.runner.redact_text(text);
        let slice = transcript::read_slice(
            Path::new(context.session_path),
            watermark.as_deref(),
            &redact,
        )
        .map_err(Error::Read)?;

        let mut row = RunRow {
            id: run_id.clone(),
            session_id: session_id.to_owned(),
            trigger: trigger.as_str().to_owned(),
            started_at,
            finished_at: String::new(),
            status: Status::Noop,
            from_entry: slice.after_id.clone(),
            to_entry: slice.last_id.clone(),
            input_bytes: 0,
            truncated: false,
            memories_proposed: 0,
            memories_dropped: 0,
            skills_written: 0,
            detail: String::new(),
        };
        let mut report = Report {
            run_id: run_id.clone(),
            status: Status::Noop,
            candidates: Vec::new(),
            dropped: BTreeMap::new(),
            skills: Vec::new(),
            skill_approvals: Vec::new(),
            skills_withheld: false,
            entries: slice.entries.len(),
            tainted: slice.tainted(),
            truncated: false,
            note: String::new(),
            items: Vec::new(),
            external: slice
                .entries
                .iter()
                .filter(|entry| entry.external)
                .map(|entry| {
                    let source = if entry.tool.is_empty() {
                        entry.role.as_str()
                    } else {
                        entry.tool.as_str()
                    };
                    (entry.id.clone(), source.to_owned())
                })
                .collect(),
            skills_note: "",
            log: None,
            retrospective: String::new(),
        };

        let user_turns = slice
            .entries
            .iter()
            .filter(|entry| entry.role == EntryRole::User && !entry.external)
            .count();
        let has_user = user_turns > 0;
        if trigger == Trigger::OnExit && user_turns < self.config.min_turns {
            // Too short to be worth a call. No row and no watermark: the
            // session is ending, and `/reflect` can still cover it later.
            return Ok(Report::skipped("the session is shorter than min_turns"));
        }
        let skills_allowed = self.skills_allowed(context, &slice, &mut report);
        if !has_user || (!self.config.memories && !skills_allowed) {
            return self.finish(context, row, report, "", "");
        }

        let existing = existing_memories(context)?;
        let (shown, truncated) = prompt::fit(&slice.entries, self.config.max_input_bytes);
        let skill_context = skills_allowed.then(|| self.skill_context(context));
        let message = prompt::user_message(
            shown,
            &existing,
            skill_context.as_deref(),
            &bounded_focus(focus),
        );
        row.input_bytes = message.len();
        row.truncated = truncated;
        report.truncated = truncated;

        if !matches!(trigger, Trigger::Manual | Trigger::Requested)
            && let Some(gate) = &self.gate
        {
            // Reuse only the bounded, redacted evidence. No standing memory,
            // skill contents or model-controlled focus is needed by this hint.
            let state = prompt::user_message(shown, &[], None, "");
            match gate.decide(&state, trigger.as_str(), cancel).await {
                gate::Outcome::Defer => {
                    return Ok(Report::skipped(
                        "automatic reflection deferred; evidence retained",
                    ));
                }
                gate::Outcome::Cancelled => {
                    row.status = Status::Canceled;
                    let _ = self.finish(context, row, report, "", "");
                    return Err(Error::Cancelled);
                }
                gate::Outcome::Immediate => {}
            }
        }

        let request = TextRequest {
            system_prompt: &prompt::system_prompt(self.config.memories, skills_allowed),
            user_text: &message,
            maximum_bytes: MAXIMUM_ANSWER_BYTES,
        };
        let task_id = format!("reflection:{run_id}");
        let answer = match context
            .runner
            .complete_text(&request, &task_id, cancel)
            .await
        {
            Ok(response) => response.text,
            Err(error) if error.is_cancelled() => {
                row.status = Status::Canceled;
                let _ = self.finish(context, row, report, "", "");
                return Err(Error::Cancelled);
            }
            Err(error) => {
                row.status = Status::Failed;
                row.detail = "model_call".into();
                let message = error.to_string();
                let _ = self.finish(context, row, report, &message, "");
                return Err(Error::Model(message));
            }
        };

        let plan = match output::plan(
            &answer,
            shown,
            &existing,
            if self.config.memories {
                self.config.max_memories
            } else {
                0
            },
            skills_allowed.then_some(self.config.max_skills),
        ) {
            Ok(plan) => plan,
            Err(message) => {
                row.status = Status::Failed;
                row.detail = "invalid_answer".into();
                let _ = self.finish(context, row, report, &message, &answer);
                return Err(Error::InvalidAnswer(message));
            }
        };
        report.dropped = plan.dropped.clone();
        report.retrospective = runlog::clean(&plan.retrospective, &redact);
        let skill_candidates = plan.skills.clone();
        let rejected = plan.rejected.clone();
        self.apply(context, &run_id, plan, &mut report, cancel)
            .await;
        if let Err(Error::Cancelled) = self
            .apply_skills(
                context,
                &run_id,
                shown,
                skill_candidates,
                cancel,
                &mut report,
            )
            .await
        {
            row.status = Status::Canceled;
            row.memories_proposed = report.candidates.len();
            report.items.extend(rejected);
            let _ = self.finish(context, row, report, "", "");
            return Err(Error::Cancelled);
        }

        row.status = Status::Ok;
        row.memories_proposed = report.candidates.len();
        row.memories_dropped = report.dropped.values().sum();
        row.skills_written = report.skills.len();
        report.status = Status::Ok;
        report.items.extend(rejected);
        self.finish(context, row, report, "", "")
    }

    /// Whether this run asks the model for skills: skills are on, there is a
    /// place to write them, and the slice is clean enough
    /// (`skill_source`). Records a withheld run on the report.
    fn skills_allowed(
        &self,
        context: &Context<'_>,
        slice: &transcript::Slice,
        report: &mut Report,
    ) -> bool {
        if !self.config.skills {
            report.skills_note = "[reflection].skills is false";
            return false;
        }
        if context.skill_roots.is_none() || self.store.is_none() {
            report.skills_note = "skills are disabled ([skills].enabled) or there is no skill directory or reflection store";
            return false;
        }
        if slice.tainted() && self.config.skill_source == SkillSource::Untainted {
            report.skills_withheld = true;
            report.skills_note = "the slice has external entries and skill_source = \"untainted\"";
            return false;
        }
        true
    }

    /// The skills the model is shown: every discovered skill's name and
    /// description, and the body of the ones reflection owns, which are the
    /// only ones it may revise.
    fn skill_context(&self, context: &Context<'_>) -> Vec<prompt::SkillInfo> {
        let Some(roots) = context.skill_roots else {
            return Vec::new();
        };
        context
            .runner
            .skills()
            .skills()
            .iter()
            .map(|skill| {
                let owned = self.owns(roots, &skill.name);
                let body = if owned {
                    crate::skill::load(skill).ok()
                } else {
                    None
                };
                let scripts = if owned {
                    skillwrite::scripts(roots, &skill.name).unwrap_or_default()
                } else {
                    Default::default()
                };
                prompt::SkillInfo {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    owned,
                    body,
                    scripts,
                }
            })
            .collect()
    }

    /// Whether reflection still owns `name`: it wrote it and no one has
    /// changed the file since.
    fn owns(&self, roots: &skillwrite::Roots, name: &str) -> bool {
        let Some(row) = self.generated_skill(name) else {
            return false;
        };
        skillwrite::still_owned(roots, &row)
    }

    pub fn attach_approvals(
        &self,
        service: &Arc<crate::approval::Service>,
        workspace: &str,
        roots: skillwrite::Roots,
    ) -> Result<(), String> {
        if let Some(store) = &self.store {
            service
                .register_domain(
                    workspace,
                    "reflection_skill",
                    Arc::new(crate::approval::reflection::Adapter {
                        store: store.clone(),
                        roots,
                        maximum: self.config.max_generated_skills,
                        require_review: self.config.skill_review,
                    }),
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Runs each skill candidate through the rest of the vetting pipeline
    /// (structure and rule scan, then the model review) and writes the ones
    /// that pass. Each failure is counted on the report by reason.
    async fn apply_skills(
        &self,
        context: &Context<'_>,
        run_id: &str,
        shown: &[transcript::Entry],
        candidates: Vec<guard::Candidate>,
        cancel: &CancellationToken,
        report: &mut Report,
    ) -> Result<(), Error> {
        let (Some(store), Some(roots)) = (&self.store, context.skill_roots) else {
            return Ok(());
        };
        let by_id: std::collections::HashMap<&str, &transcript::Entry> = shown
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect();
        let redact = |text: &str| context.runner.redact_text(text);
        let mut remaining = candidates.into_iter();
        while let Some(candidate) = remaining.next() {
            let item = skill_item(&candidate);
            let reject = |report: &mut Report, reason: &'static str| {
                *report.dropped.entry(reason).or_default() += 1;
                let mut item = item.clone();
                item.outcome = runlog::Outcome::Dropped(reason);
                report.items.push(item);
            };
            let checked = match guard::check(candidate, &by_id, &redact) {
                Ok(checked) => checked,
                Err(reason) => {
                    reject(report, reason);
                    continue;
                }
            };
            // Refuse what cannot be written before spending a review call.
            let candidate = checked.candidate();
            if let Err(reason) = skillwrite::precheck(
                store.as_ref(),
                roots,
                candidate.action,
                &candidate.name,
                self.config.max_generated_skills,
            ) {
                reject(report, reason);
                continue;
            }
            if let Err(reason) = skillwrite::validate_python(&candidate.scripts) {
                reject(report, reason);
                continue;
            }
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let verdict = if self.config.skill_review {
                let task_id = format!("reflection:{run_id}:review");
                match review::review(context.runner, checked.candidate(), &task_id, cancel).await {
                    Ok(review::Outcome::Allow) => guard::Verdict::Allowed,
                    Ok(review::Outcome::Reject(_)) => {
                        reject(report, "review_rejected");
                        continue;
                    }
                    Err(review::Failure::Cancelled) => {
                        // Not counted in `dropped`: a canceled run is retried.
                        let mut current = item.clone();
                        current.outcome = runlog::Outcome::Dropped("canceled");
                        report.items.push(current);
                        for candidate in remaining {
                            let mut item = skill_item(&candidate);
                            item.outcome = runlog::Outcome::Dropped("canceled");
                            report.items.push(item);
                        }
                        return Err(Error::Cancelled);
                    }
                    Err(review::Failure::Unavailable(_)) => {
                        reject(report, "review_failed");
                        continue;
                    }
                }
            } else {
                guard::Verdict::NotRequested
            };
            let vetted = guard::approve(checked, verdict);
            if let Some(approvals) = context.approvals {
                match crate::approval::reflection::publish(
                    approvals,
                    store.clone(),
                    roots,
                    self.config.max_generated_skills,
                    &context.runner.session().header().workspace,
                    context.session_id,
                    context.session_path,
                    context.presentation_origin,
                    run_id,
                    &vetted,
                    self.config.skill_review,
                ) {
                    Ok(request) => {
                        let mut item = item.clone();
                        item.outcome = runlog::Outcome::Approval(request.id.clone());
                        report.items.push(item);
                        report.skill_approvals.push(request.id);
                    }
                    Err(_) => reject(report, "skill_approval_failed"),
                }
                continue;
            }
            reject(report, "skill_approval_unavailable");
        }
        Ok(())
    }

    /// Queues each proposal as a candidate, dropping what the store refuses
    /// and what is already pending for review. The runner's advisor, when it
    /// has one, asks for review hints on all proposals concurrently first; a
    /// canceled run queues nothing further.
    async fn apply(
        &self,
        context: &Context<'_>,
        run_id: &str,
        plan: Plan,
        report: &mut Report,
        cancel: &CancellationToken,
    ) {
        let seen = known_candidates(context);
        let mut requests = Vec::new();
        // Indexes into `report.items` of the proposals sent to the store, in
        // request order.
        let mut sent = Vec::new();
        for proposal in plan.proposals {
            let scope = match proposal.scope {
                ScopeChoice::User => context.user_scope.clone(),
                ScopeChoice::Workspace => context.workspace_scope.clone(),
            };
            let mut item = memory_item(&proposal);
            if seen.iter().any(|known| known.matches(&scope, &proposal)) {
                *report.dropped.entry("duplicate_candidate").or_default() += 1;
                item.outcome = runlog::Outcome::Dropped("duplicate_candidate");
                report.items.push(item);
                continue;
            }
            requests.push(propose_request(context, run_id, scope, &proposal));
            sent.push(report.items.len());
            report.items.push(item);
        }
        let results = match &context.runner.advisor {
            Some(advisor) => {
                match advisor
                    .propose(context.service, context.user_scope, requests, cancel)
                    .await
                {
                    Some(results) => results,
                    None => {
                        for index in sent {
                            report.items[index].outcome = runlog::Outcome::Dropped("canceled");
                        }
                        return;
                    }
                }
            }
            None => requests
                .iter()
                .map(|request| context.service.propose(request))
                .collect(),
        };
        for (index, result) in sent.into_iter().zip(results) {
            match result {
                Ok(candidates) => {
                    let ids: Vec<String> = candidates
                        .into_iter()
                        .map(|candidate| candidate.id)
                        .collect();
                    report.items[index].outcome = runlog::Outcome::Queued(ids.join(", "));
                    report.candidates.extend(ids);
                }
                Err(_) => {
                    *report.dropped.entry("store_rejected").or_default() += 1;
                    report.items[index].outcome = runlog::Outcome::Dropped("store_rejected");
                }
            }
        }
    }

    /// Records the run row, then writes the run log when a log directory is
    /// set. A log that cannot be written is reported on the report and does
    /// not fail the run. `error` is the failure text and `answer` the raw
    /// model answer of an invalid answer; both are empty otherwise.
    fn finish(
        &self,
        context: &Context<'_>,
        mut row: RunRow,
        mut report: Report,
        error: &str,
        answer: &str,
    ) -> Result<Report, Error> {
        row.finished_at = now();
        if let Some(store) = &self.store {
            store
                .record(&row)
                .map_err(|error| Error::Store(error.to_string()))?;
        }
        if let Some(dir) = &self.log_dir {
            let redact = |text: &str| context.runner.redact_text(text);
            let content = runlog::render(
                &runlog::Input {
                    row: &row,
                    session_path: context.session_path,
                    error,
                    answer,
                    entries: report.entries,
                    truncated: row.truncated,
                    external: &report.external,
                    skills_note: report.skills_note,
                    items: &report.items,
                    retrospective: &report.retrospective,
                },
                &redact,
            );
            report.log = Some(runlog::write(dir, &row.started_at, &row.id, &content));
        }
        Ok(report)
    }
}

fn memory_item(proposal: &Proposal) -> runlog::Item {
    let action = match proposal.action {
        Action::Create => "create",
        Action::Update => "update",
        Action::Forget => "forget",
    };
    let title = if proposal.target_id.is_empty() {
        format!(
            "{action} {} {} `{}`",
            proposal.scope.as_str(),
            proposal.kind,
            proposal.key
        )
    } else {
        format!(
            "{action} {} {} `{}` (target {})",
            proposal.scope.as_str(),
            proposal.kind,
            proposal.key,
            proposal.target_id
        )
    };
    runlog::Item {
        kind: runlog::Kind::Memory,
        title,
        text: proposal.text.clone(),
        body: String::new(),
        reason: proposal.reason.clone(),
        cited: proposal.cited.clone(),
        outcome: runlog::Outcome::Dropped(""),
    }
}

fn skill_item(candidate: &guard::Candidate) -> runlog::Item {
    let action = match candidate.action {
        output::SkillAction::Create => "create",
        output::SkillAction::Revise => "revise",
    };
    runlog::Item {
        kind: runlog::Kind::Skill,
        title: format!("{action} skill `{}`", candidate.name),
        text: candidate.description.clone(),
        body: guard::content(&candidate.body, &candidate.scripts),
        reason: candidate.reason.clone(),
        cited: candidate.cited.clone(),
        outcome: runlog::Outcome::Dropped(""),
    }
}

/// The skill roots for a run: write to `~/.otto/skills`, keep history in
/// `~/.otto/skill-history`, and treat every configured root as taken. `None`
/// without a home directory.
pub fn skill_roots(home: &str, configured: &[String]) -> Option<skillwrite::Roots> {
    if home.is_empty() {
        return None;
    }
    let base = Path::new(home).join(".otto");
    Some(skillwrite::Roots {
        write_root: base.join("skills"),
        history_root: base.join("skill-history"),
        lookup_roots: configured.iter().map(std::path::PathBuf::from).collect(),
    })
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn bounded_focus(focus: &str) -> String {
    let cleaned: String = focus
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut cleaned = cleaned.trim().to_owned();
    while cleaned.len() > prompt::MAXIMUM_FOCUS_BYTES {
        cleaned.pop();
    }
    cleaned
}

fn existing_memories(context: &Context<'_>) -> Result<Vec<Existing>, Error> {
    let mut existing = Vec::new();
    for (choice, scope) in [
        (ScopeChoice::User, context.user_scope),
        (ScopeChoice::Workspace, context.workspace_scope),
    ] {
        let page = context
            .service
            .list(&crate::memory::ListRequest {
                all_scopes: false,
                scopes: vec![scope.clone()],
                kinds: Vec::new(),
                labels: Vec::new(),
                limit: CONTEXT_LIMIT,
                cursor: String::new(),
                now: Utc::now(),
                include_expired: false,
            })
            .map_err(|_| Error::MemoryUnavailable)?;
        existing.extend(page.records.into_iter().map(|record| Existing {
            id: record.id,
            scope: choice,
            kind: record.kind,
            key: record.key,
            text: record.text,
            revision: record.revision,
        }));
    }
    Ok(existing)
}

/// A pending candidate, for duplicate suppression. The store clears a
/// rejected candidate's content, so a rejected proposal cannot be recognized
/// and may be proposed again.
struct Known {
    scope: Scope,
    action: CandidateAction,
    kind: String,
    key: String,
    text: String,
    target_id: String,
}

impl Known {
    fn matches(&self, scope: &Scope, proposal: &Proposal) -> bool {
        let action = match proposal.action {
            Action::Create => CandidateAction::Create,
            Action::Update => CandidateAction::Update,
            Action::Forget => CandidateAction::Forget,
        };
        self.scope == *scope
            && self.action == action
            && self.kind == proposal.kind
            && self.key == proposal.key
            && self.text == proposal.text
            && self.target_id == proposal.target_id
    }
}

/// Best effort: a failure to read candidates means no suppression, never a
/// failed run.
fn known_candidates(context: &Context<'_>) -> Vec<Known> {
    let result = context.service.search(&SearchRequest {
        scopes: vec![context.user_scope.clone(), context.workspace_scope.clone()],
        include_candidates: true,
        candidate_states: vec![CandidateState::Pending],
        limit: CONTEXT_LIMIT,
        token_budget: 1,
        now: Utc::now(),
        ..SearchRequest::default()
    });
    result
        .map(|result| {
            result
                .candidates
                .into_iter()
                .map(|candidate| Known {
                    scope: candidate.proposed.scope,
                    action: candidate.action,
                    kind: candidate.proposed.kind,
                    key: candidate.proposed.key,
                    text: candidate.proposed.text,
                    target_id: candidate.target_id,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn propose_request(
    context: &Context<'_>,
    run_id: &str,
    scope: Scope,
    proposal: &Proposal,
) -> ProposeRequest {
    ProposeRequest {
        action: match proposal.action {
            Action::Create => CandidateAction::Create,
            Action::Update => CandidateAction::Update,
            Action::Forget => CandidateAction::Forget,
        },
        scope,
        kind: proposal.kind.clone(),
        key: proposal.key.clone(),
        text: proposal.text.clone(),
        confidence: proposal.confidence,
        target_id: proposal.target_id.clone(),
        base_revision: proposal.base_revision,
        reason: proposal.reason.clone(),
        source: Provenance {
            origin: Some(Origin::Extractor),
            session_id: context.session_id.to_owned(),
            message_ids: proposal.cited.clone(),
            ..Provenance::default()
        },
        // The store refuses an observation id on a candidate, so the run id
        // travels as metadata for tracing a candidate back to its run.
        metadata: BTreeMap::from_iter(
            [(RUN_METADATA_KEY.to_owned(), run_id.to_owned())]
                .into_iter()
                .chain(context.presentation_origin.map(|origin| {
                    (
                        crate::memory::PRESENTATION_ORIGIN_METADATA.to_owned(),
                        origin.as_str().to_owned(),
                    )
                })),
        ),
        ..ProposeRequest::default()
    }
}

#[cfg(test)]
mod tests;
