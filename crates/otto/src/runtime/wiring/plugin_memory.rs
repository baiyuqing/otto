//! The per-runner memory client of a `plugin:<package>` memory source.

use super::*;

impl Builder {
    /// The memory client of one runner. The file store and an injected
    /// extension share the client opened at startup; a plugin source starts
    /// the package's memory command here, where the runner's plugin runtime
    /// exists. A failure is an error when the source is `required`, otherwise
    /// a warning and no memory.
    pub(crate) async fn runner_memory_client(
        &self,
        plugins: &crate::plugin::Snapshot,
        extensions: &Arc<crate::plugin::hooks::Runtime>,
        runtime: &Runtime,
        warnings: &mut (dyn Write + Send),
    ) -> Result<Option<Arc<crate::memory::client::Client>>, BuildError> {
        let Some(selection) = &self.memory.plugin else {
            return Ok(self.memory.client.clone());
        };
        if !self.memory.usable || !self.boundary_allows_dynamic(Some(runtime)) {
            return Ok(None);
        }
        match self
            .plugin_memory_client(selection, plugins, extensions, runtime)
            .await
        {
            Ok(client) => Ok(Some(Arc::new(client))),
            Err(reason) => {
                let reason = self.redact_error(&reason, Some(runtime));
                if selection.required {
                    return Err(reason);
                }
                let _ = writeln!(warnings, "warning: {reason}; continuing without memory");
                Ok(None)
            }
        }
    }

    async fn plugin_memory_client(
        &self,
        selection: &PluginMemorySelection,
        plugins: &crate::plugin::Snapshot,
        extensions: &Arc<crate::plugin::hooks::Runtime>,
        runtime: &Runtime,
    ) -> Result<crate::memory::client::Client, String> {
        let source = extensions
            .memory_source(plugins, &selection.package, &CancellationToken::new())
            .await?;
        let grant = otto_core::memory::LearningGrant {
            scopes: vec![
                neutral_scope(&self.memory.user_scope),
                neutral_scope(&self.workspace_scope),
            ],
            input_classes: ["message", "tool_result", "explicit_memory"]
                .map(String::from)
                .to_vec(),
            background: true,
        };
        let guard = crate::memory::guard::content_guard(&self.secret_values(Some(runtime)))?;
        let client = crate::memory::client::Client::with_learning(Arc::new(source), grant)
            .and_then(|client| client.with_guard(Arc::new(guard)))
            .map_err(|error| error.to_string())?;
        if selection.require_encryption && client.describe().encryption_at_rest != Some(true) {
            return Err("selected memory service does not guarantee encryption at rest".into());
        }
        Ok(client)
    }
}
