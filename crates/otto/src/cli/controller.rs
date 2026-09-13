//! The session-lifecycle owner the line frontend talks to.
//!
//! Port of the part of `internal/app.Controller` that the REPL uses: the
//! current session and runner, and the operations that replace them (`/new`,
//! `/archive`, `/model <profile>`). Everything else `internal/app` owns is a
//! later phase: memory, sub-agent tasks, wake turns, the session browser, and
//! the sandbox reloader.
//!
//! ponytail: there is no `Backend` trait. The REPL is the only consumer and
//! there is exactly one implementation, so it takes a `&Controller` directly.
//! A trait goes in when `internal/server` and the TUI arrive and need one.
//!
//! Concurrency: `state` is a plain mutex holding the current triple. Every
//! method takes it, clones the `Arc<Runner>`, and releases it before awaiting,
//! so no lock is ever held across an await point. Go's `ErrPromptActive`
//! guard is not ported: the line frontend runs one turn at a time by
//! construction.
//!
//! Errors: every failure is a already-redacted `String`, built through
//! [`Builder::redact_error`] wherever the text could carry a host path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use otto_core::agent::{AgentError, CompactionResult, EventSink};
use otto_core::config::resolve::Runtime;
use otto_core::session::RuntimeMetadata;
use tokio_util::sync::CancellationToken;

use crate::session::{ArchiveResult, archive};

use super::info::SandboxInfo;
use super::runtime_builder::{Builder, Runner, RuntimeInfo, SharedSession};

/// Go's `app.ErrPersistenceDisabled`.
pub const PERSISTENCE_DISABLED: &str = "session persistence is disabled";
/// Go's `app.ErrProfileSwitchUnavailable`.
pub const PROFILE_SWITCH_UNAVAILABLE: &str = "profile switching is not available";
/// Go's `app.ErrSandboxReloadUnavailable`. `/sandbox reload` is a later phase.
pub const SANDBOX_RELOAD_UNAVAILABLE: &str = "sandbox reload is not available";
/// Go's `errSessionOperationUnavailable` in `cmd/otto/runtime_builder.go`.
pub const SESSION_OPERATION_UNAVAILABLE: &str = "session operation is unavailable";

/// What a frontend may display. Port of the `app.Info` fields the REPL reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub sandbox: SandboxInfo,
}

/// The session, runner and resolved runtime currently in force.
struct Current {
    session: SharedSession,
    runner: Arc<Runner>,
    info: RuntimeInfo,
}

pub struct Controller {
    builder: Builder,
    /// False when the redaction boundary is closed. Every operation that
    /// would persist or display provider identity is then refused, matching
    /// Go's `dynamicContent` gate.
    dynamic_content: bool,
    state: Mutex<Current>,
}

impl Controller {
    pub fn new(
        builder: Builder,
        dynamic_content: bool,
        session: SharedSession,
        runner: Runner,
        info: RuntimeInfo,
    ) -> Self {
        Self {
            builder,
            dynamic_content,
            state: Mutex::new(Current {
                session,
                runner: Arc::new(runner),
                info,
            }),
        }
    }

    fn current(&self) -> std::sync::MutexGuard<'_, Current> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.builder.config_path
    }

    pub fn sandbox_info(&self) -> SandboxInfo {
        self.builder.effective_sandbox_info()
    }

    pub fn dynamic_content(&self) -> bool {
        self.dynamic_content
    }

    /// Port of `Controller.Info`. A closed boundary reports the sandbox only.
    pub fn info(&self) -> Info {
        let sandbox = self.sandbox_info();
        if !self.dynamic_content {
            return Info {
                sandbox,
                ..Info::default()
            };
        }
        let current = self.current();
        let header = current.session.header();
        Info {
            session_id: header.id,
            session_name: current.session.name(),
            session_path: current.session.path(),
            provider: current.info.provider.clone(),
            profile: current.info.profile.clone(),
            model: current.info.model.clone(),
            sandbox,
        }
    }

    /// The session currently in force. Used by tests and by the interop
    /// fixture writer, which need to append to whatever session is live.
    pub fn current_session(&self) -> SharedSession {
        self.current().session.clone()
    }

    /// The system prompt the current runner was built with. Used by the
    /// end-to-end test and by `--approve` diagnostics.
    pub fn system_prompt(&self) -> String {
        self.current().runner.system_prompt().to_string()
    }

    pub async fn prompt(
        &self,
        text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let runner = Arc::clone(&self.current().runner);
        runner.run(text, emit, cancel).await
    }

    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let runner = Arc::clone(&self.current().runner);
        runner.compact(focus, emit, cancel).await
    }

    /// The configured profile names, sorted. Port of `profileNames`.
    pub fn profiles(&self) -> Vec<String> {
        if !self.dynamic_content {
            return Vec::new();
        }
        let mut names: Vec<String> = self.builder.config.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    /// Port of `persistDefaultProfile`.
    pub fn set_default_profile(&self, profile: &str) -> Result<(), String> {
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        crate::config::set_default_profile_file(&self.builder.config_path, profile)
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    /// Port of `Controller.RenameSession`.
    pub fn rename_session(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("session is invalid: session name is required".to_string());
        }
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let session = self.current().session.clone();
        session
            .rename(name)
            .map_err(|error| self.builder.redact_error(&error, None))
    }

    /// Port of `Controller.NewSession` plus `buildNewReplacement`.
    pub async fn new_session(&self) -> Result<(), String> {
        let runtime = self.replacement_runtime(&self.current_metadata())?;
        let replacement = self.fresh_replacement(&runtime).await?;
        self.swap(replacement);
        Ok(())
    }

    /// Port of `Controller.SwitchProfile` plus `buildProfileReplacement`.
    pub async fn switch_profile(&self, profile: &str) -> Result<(), String> {
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let runtime = self.builder.resolve_profile(profile)?;
        if !self.builder.boundary_allows_dynamic(Some(&runtime)) {
            return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
        }
        let replacement = self.fresh_replacement(&runtime).await?;
        self.swap(replacement);
        Ok(())
    }

    /// Port of `Controller.ArchiveCurrentSession`.
    ///
    /// The replacement is built before the archive move, so every failure
    /// path leaves the current session intact. The move is the last and only
    /// committed state change.
    pub async fn archive_current_session(&self) -> Result<ArchiveResult, String> {
        let (path, metadata) = {
            let current = self.current();
            (current.session.path(), metadata_of(&current.info))
        };
        if path.is_empty() {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let runtime = self.replacement_runtime(&metadata)?;
        let replacement = self.fresh_replacement(&runtime).await?;
        let result = archive(
            &self.builder.session_root,
            &self.builder.workspace_path,
            Path::new(&path),
        );
        match result {
            Ok(result) => {
                self.swap(replacement);
                Ok(result)
            }
            Err(error) => {
                discard(replacement);
                Err(self
                    .builder
                    .redact_error(&error.to_string(), Some(&runtime)))
            }
        }
    }

    /// Closes the current session and runner. Idempotent only in the sense
    /// that both underlying closes are.
    pub fn close(&self) -> Result<(), String> {
        let current = self.current();
        current.runner.close();
        current.session.close()
    }

    fn current_metadata(&self) -> RuntimeMetadata {
        metadata_of(&self.current().info)
    }

    /// The two gates Go applies before every replacement: persistence must be
    /// enabled, and the boundary must be open both before and after resolving.
    fn replacement_runtime(&self, metadata: &RuntimeMetadata) -> Result<Runtime, String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        if !self.builder.boundary_allows_dynamic(None) {
            return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
        }
        let runtime = self.builder.resolve_session(metadata)?;
        if !self.builder.boundary_allows_dynamic(Some(&runtime)) {
            return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
        }
        Ok(runtime)
    }

    /// Port of `freshReplacement`: a new session and runner for an
    /// already-resolved runtime. Every failure closes what it built.
    async fn fresh_replacement(&self, runtime: &Runtime) -> Result<Current, String> {
        let session = self.builder.create_session(runtime)?;
        let runner = match self.builder.build_runner(&session, runtime).await {
            Ok(runner) => Arc::new(runner),
            Err(error) => {
                let _ = session.close();
                return Err(error);
            }
        };
        if let Err(error) = self.builder.update_session_runtime(&session, runtime) {
            runner.close();
            let _ = session.close();
            return Err(error);
        }
        Ok(Current {
            session,
            runner,
            info: self.builder.runtime_info(runtime),
        })
    }

    fn swap(&self, replacement: Current) {
        let previous = std::mem::replace(&mut *self.current(), replacement);
        discard(previous);
    }
}

fn discard(current: Current) {
    current.runner.close();
    let _ = current.session.close();
}

fn metadata_of(info: &RuntimeInfo) -> RuntimeMetadata {
    RuntimeMetadata {
        profile: info.profile.clone(),
        provider: info.provider.clone(),
        model: info.model.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::testutil::{builder, controller, initial_runtime, user};
    use otto_core::session::Session;

    #[tokio::test]
    async fn info_reports_the_current_session_and_the_resolved_profile() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let info = controller.info();
        assert_eq!(info.profile, "alpha");
        assert_eq!(info.model, "gpt-alpha");
        assert_eq!(info.provider, "openai-compatible");
        assert_eq!(info.session_id.len(), 32);
        // The file is lazy, so nothing exists until the first message.
        assert_eq!(info.session_path, "");
        assert_eq!(info.sandbox.summary(), controller.sandbox_info().summary());
    }

    #[tokio::test]
    async fn new_session_replaces_the_session_and_keeps_the_runtime() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let before = controller.info();

        controller.new_session().await.expect("new session");

        let after = controller.info();
        assert_ne!(after.session_id, before.session_id);
        assert_eq!(after.model, "gpt-alpha");
        assert_eq!(after.profile, "alpha");
    }

    #[tokio::test]
    async fn switching_profile_switches_the_model() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        assert_eq!(controller.profiles(), vec!["alpha", "beta"]);

        controller.switch_profile("beta").await.expect("switch");

        let info = controller.info();
        assert_eq!(info.profile, "beta");
        assert_eq!(info.model, "gpt-beta");
        assert_ne!(info.session_id, "");
    }

    #[tokio::test]
    async fn an_unknown_profile_leaves_the_session_in_place() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let before = controller.info();

        let error = controller
            .switch_profile("missing")
            .await
            .expect_err("unknown profile");
        assert!(error.contains("not found"), "{error}");
        assert_eq!(controller.info().session_id, before.session_id);
    }

    #[tokio::test]
    async fn renaming_records_the_name_on_the_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        controller
            .rename_session("  release notes  ")
            .expect("rename");

        assert_eq!(controller.info().session_name, "release notes");
        assert!(controller.rename_session("   ").is_err());
    }

    #[tokio::test]
    async fn archiving_moves_the_file_and_starts_a_fresh_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        // The store is lazy: one appended message gives the session a file.
        {
            let session = controller.current().session.clone();
            session.append(user("hello")).await.expect("append");
        }
        let before = controller.info();
        assert!(!before.session_path.is_empty());

        let result = controller.archive_current_session().await.expect("archive");

        assert!(result.path.contains("archive"), "{}", result.path);
        assert!(Path::new(&result.path).exists());
        assert!(!Path::new(&before.session_path).exists());
        assert_ne!(controller.info().session_id, before.session_id);
    }

    #[tokio::test]
    async fn a_session_with_no_file_cannot_be_archived() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let error = controller
            .archive_current_session()
            .await
            .expect_err("no file");
        assert_eq!(error, PERSISTENCE_DISABLED);
    }

    #[tokio::test]
    async fn the_default_profile_is_written_to_the_configuration_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        std::fs::write(
            controller.config_path(),
            "default_profile = \"alpha\"\n\n[profiles.alpha]\n\n[profiles.beta]\n",
        )
        .expect("write config");

        controller.set_default_profile("beta").expect("set default");

        let content = std::fs::read_to_string(controller.config_path()).expect("read config");
        assert!(content.contains("default_profile = \"beta\""), "{content}");
    }

    #[tokio::test]
    async fn a_closed_boundary_refuses_every_replacement() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let builder = builder(workspace.path(), sessions.path());
        let runtime = initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        let controller = Controller::new(builder, false, session, runner, info);

        assert_eq!(controller.profiles(), Vec::<String>::new());
        assert_eq!(controller.info().model, "");
        assert_eq!(
            controller.new_session().await.expect_err("new"),
            PERSISTENCE_DISABLED
        );
        assert_eq!(
            controller.switch_profile("beta").await.expect_err("switch"),
            PROFILE_SWITCH_UNAVAILABLE
        );
        assert_eq!(
            controller.set_default_profile("beta").expect_err("default"),
            PROFILE_SWITCH_UNAVAILABLE
        );
        assert_eq!(
            controller.rename_session("name").expect_err("rename"),
            PERSISTENCE_DISABLED
        );
    }
}
