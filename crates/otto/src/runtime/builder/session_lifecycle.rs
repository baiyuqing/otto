//! Builder session resolution, creation and forking.

use super::*;

impl Builder {
    /// The text a caller may print for `message`.
    ///
    /// A closed boundary yields the empty string, deliberately, so no
    /// diagnostic escapes when the redaction set is incomplete.
    pub fn redact_error(&self, message: &str, runtime: Option<&Runtime>) -> String {
        let redactor = boundary::secret_redactor(&self.boundary_inputs(), runtime);
        if !redactor.allows_dynamic_content() {
            return String::new();
        }
        redactor.redact_string(message)
    }

    /// Resolves a replacement runtime from the provenance a session carries.
    pub fn resolve_session(&self, metadata: &RuntimeMetadata) -> Result<Runtime, BuildError> {
        // The stored `profile` field holds a selector for sessions written
        // since provider/model configuration; older sessions hold a profile
        // name, and otto-core resolves those from the recorded type and model.
        let overrides = Overrides {
            model: String::new(),
            provider: String::new(),
            legacy_profile: String::new(),
            ..self.overrides.clone()
        };
        otto_core::config::resolve::resolve(
            &self.config,
            &resume_environment(&self.environment),
            &session_defaults(metadata),
            &overrides,
        )
        .map_err(|error| self.redact_error(&error.to_string(), None))
    }

    /// The selector is an explicit override, so its provider, model and
    /// endpoint win over command-line overrides.
    pub fn resolve_model(&self, selector: &str) -> Result<Runtime, BuildError> {
        let overrides = Overrides {
            model: selector.to_string(),
            provider: String::new(),
            legacy_profile: String::new(),
            base_url: String::new(),
            ..self.overrides.clone()
        };
        otto_core::config::resolve::resolve(
            &self.config,
            &resume_environment(&self.environment),
            &SessionDefaults::default(),
            &overrides,
        )
        .map_err(|error| self.redact_error(&error.to_string(), None))
    }

    /// Records the resolved provenance on the session when it differs from the
    /// header's.
    pub fn update_session_runtime(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Result<(), BuildError> {
        if !self.boundary_allows_dynamic(Some(runtime)) {
            return Ok(());
        }
        let header = session.header();
        let selector = runtime.model_selector.to_string();
        if header.profile == selector
            && header.provider == runtime.provider_type
            && header.model == runtime.model
        {
            return Ok(());
        }
        session.update_runtime(&RuntimeMetadata {
            profile: selector,
            provider: runtime.provider_type.clone(),
            model: runtime.model.clone(),
        })
    }

    /// `Store::create_lazy` defers the file until the first user message, so
    /// starting Otto and quitting without a prompt leaves nothing behind.
    pub fn create_session(&self, runtime: &Runtime) -> Result<SharedSession, BuildError> {
        let id = random_id().map_err(|error| format!("create session id: {error}"))?;
        self.create_session_with_id(runtime, id)
    }

    pub fn create_session_with_id(
        &self,
        runtime: &Runtime,
        id: String,
    ) -> Result<SharedSession, BuildError> {
        let header = Header {
            version: CURRENT_VERSION,
            id,
            workspace: self.workspace_path.clone(),
            provider: runtime.provider_type.clone(),
            profile: runtime.model_selector.to_string(),
            model: runtime.model.clone(),
            created_at: Utc::now(),
        };
        if self.no_session {
            return Ok(SharedSession::memory(header));
        }
        let failover_seconds = self
            .create_lease()
            .map_err(|error| self.redact_error(&error, Some(runtime)))?;
        let session_root = self
            .storage
            .session_root(&self.session_root)
            .map_err(|error| self.redact_error(&error, Some(runtime)))?;
        let handle = self
            .storage
            .sessions()
            .and_then(|repository| repository.create_lazy(&session_root, header, failover_seconds))
            .map_err(|error| self.redact_error(&error, Some(runtime)))?;
        Ok(SharedSession::new(handle))
    }
}

impl Builder {
    /// Writes a new session forked from the session file `reference`; see
    /// [`crate::session::SessionRepository::fork`].
    pub fn fork_session_file(
        &self,
        reference: &str,
        before: Option<&str>,
    ) -> Result<crate::session::Forked, otto_core::session::ForkError> {
        let failed = |message: String| {
            otto_core::session::ForkError::Session(otto_core::session::PiError::other(message))
        };
        let id = random_id().map_err(|error| failed(format!("create session id: {error}")))?;
        self.fork_session_file_with_id(reference, before, &id, None)
    }

    pub fn fork_session_file_with_id(
        &self,
        reference: &str,
        before: Option<&str>,
        id: &str,
        request_id: Option<&str>,
    ) -> Result<crate::session::Forked, otto_core::session::ForkError> {
        let failed = |message: String| {
            otto_core::session::ForkError::Session(otto_core::session::PiError::other(message))
        };
        self.storage
            .sessions()
            .map_err(failed)?
            .fork(reference, before, id, request_id)
    }

    /// The places the session file `reference` can be forked, oldest first.
    pub fn session_fork_points(
        &self,
        reference: &str,
    ) -> Result<Vec<otto_core::session::ForkPoint>, String> {
        self.storage.sessions()?.fork_points(reference)
    }
}

/// The stored session provenance as resolution defaults: the `profile` field
/// counts as a selector only when it parses as one.
pub(crate) fn session_defaults(metadata: &RuntimeMetadata) -> SessionDefaults {
    let model_selector = if otto_core::config::ModelSelector::parse(&metadata.profile).is_ok() {
        metadata.profile.clone()
    } else {
        String::new()
    };
    SessionDefaults {
        provider: metadata.provider.clone(),
        model: metadata.model.clone(),
        model_selector,
    }
}
