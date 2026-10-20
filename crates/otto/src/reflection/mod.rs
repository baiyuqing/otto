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
}

mod apply;
mod proposal;
mod run;

pub use proposal::*;

#[cfg(test)]
mod tests;
