//! The startup composition of `otto`: environment
//! capture, layered configuration, the redaction boundary, storage, the
//! workspace, the sandbox and the memory service.
//!
//! `otto_cli::run` calls the phase functions in order and keeps its own steps
//! between them (flag-specific checks, session resume, frontend selection).
//! [`start`] runs the same phases straight through, as `otto acp` does. Every
//! warning goes to the `stderr` writer at the moment its step emits it, and
//! every error string is already redacted by the boundary.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{
    File, MemoryRuntime, SandboxSettings, resolve_mcp, resolve_memory, resolve_sandbox,
};
use otto_core::session::RuntimeMetadata;
use tokio_util::sync::CancellationToken;

use crate::auth::{CapturedAuth, capture_auth_credentials};
use crate::sandbox::direct::DirectDriver;
use crate::sandbox::environment::{EnvironmentOptions, EnvironmentSnapshot, resolve_environment};
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy};

use otto_session::workspace::confined::Workspace;

use super::SESSION_OPERATION_UNAVAILABLE;
use super::boundary::{self, BoundaryInputs};
use super::builder::{Builder, Shared, resolve_initial_runtime};
use super::sandbox_runtime::{
    OpenOptions, normalize_sandbox_runtime, open_sandbox_runtime, sandbox_runtime_warning,
    settings_from_config,
};
use super::sandbox_switch::{SandboxReloader, SandboxSwitch};
use otto_platform::fsops::{canonical_directory, canonical_executable_file};

/// Darwin's process argument/environment budget is about 1 MiB. These
/// ceilings are deliberately larger while still bounding injected snapshots.
const MAX_LOOKUP_ENVIRONMENT_NAME_BYTES: usize = 4 << 10;
const MAX_LOOKUP_ENVIRONMENT_ENTRY_BYTES: usize = 1 << 20;
const MAX_LOOKUP_ENVIRONMENT_ENTRIES: usize = 1 << 18;
const MAX_LOOKUP_ENVIRONMENT_BYTES: usize = 8 << 20;
pub const MAX_CAPTURED_ENVIRONMENT_ENTRIES: usize = 1 << 19;
pub const MAX_CAPTURED_ENVIRONMENT_BYTES: usize = 16 << 20;

const ENVIRONMENT_SNAPSHOT_TOO_LARGE: &str = "process environment snapshot is too large";

/// The process environment, parsed into names and values.
pub type EnvironmentLookup = HashMap<String, String>;

/// Builds the automatic skill contract checker when skills and the check are
/// enabled and `TYPESAFE_API_KEY` is non-empty. The check defaults to enabled
/// and is set by `[skills].typesafe_skill_check`.
/// A database open failure prints one warning here, before any UI starts, and
/// leaves the checker off for the process; `TYPESAFE_API_KEY` never appears in
/// that warning.
pub fn skill_checker(
    storage: &crate::runtime::storage::Storage,
    config: &File,
    environment: &HashMap<String, String>,
    home: &str,
    stderr: &mut (dyn Write + Send),
) -> Option<Arc<crate::skill::check::Checker>> {
    if config.skills.enabled == Some(false) {
        return None;
    }
    if !config.skills.typesafe_skill_check.unwrap_or(true) {
        return None;
    }
    let api_key = environment
        .get("TYPESAFE_API_KEY")
        .map(String::as_str)
        .unwrap_or("");
    if api_key.is_empty() {
        return None;
    }
    let db_path = Path::new(home).join(".otto/skill-checks.db");
    match storage.skill_checks(&db_path) {
        Ok(store) => Some(Arc::new(crate::skill::check::Checker::with_store(
            store,
            crate::typesafe::Client::new(crate::typesafe::BASE_URL, api_key),
        ))),
        Err(message) => {
            let _ = writeln!(
                stderr,
                "warning: skill check store unavailable ({message}), continuing without automatic contract checks"
            );
            None
        }
    }
}

/// The boundary that exists before a profile is resolved.
pub struct StartupBoundary {
    pub sandbox_secrets: Vec<String>,
    pub complete: bool,
    pub config: File,
    pub environment: HashMap<String, String>,
    pub base_url: String,
}

impl StartupBoundary {
    fn inputs(&self) -> BoundaryInputs<'_> {
        BoundaryInputs {
            sandbox_secrets: &self.sandbox_secrets,
            sandbox_secrets_complete: self.complete,
            config: &self.config,
            environment: &self.environment,
            overrides_base_url: &self.base_url,
        }
    }

    pub fn redactor(&self) -> otto_core::agent::redactor::Redactor {
        boundary::secret_redactor(&self.inputs(), None)
    }

    pub fn allows_dynamic(&self) -> bool {
        self.redactor().allows_dynamic_content()
    }

    pub fn redact(&self, message: &str) -> String {
        redact_with(&self.redactor(), message)
    }
}

/// A closed boundary renders nothing at all, deliberately: an incomplete
/// redaction set must not let a diagnostic escape.
pub fn redact_with(redactor: &otto_core::agent::redactor::Redactor, message: &str) -> String {
    if !redactor.allows_dynamic_content() {
        return String::new();
    }
    redactor.redact_string(message)
}

fn merge_redactions(first: &[String], second: &[String]) -> (Vec<String>, bool) {
    let mut collector = otto_core::safetext::SecretCollector::new();
    for group in [first, second] {
        for value in group {
            if !collector.add_form(value) {
                return (collector.values(), false);
            }
        }
    }
    (collector.values(), true)
}

mod compose;
mod environment;
mod memory_start;
mod sandbox;
mod workspace;

pub use compose::*;
pub use environment::*;
pub use memory_start::*;
pub use sandbox::*;
pub use workspace::*;

#[cfg(test)]
mod tests;
