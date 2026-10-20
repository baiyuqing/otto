//! Builder memory context, memory tools and memory binding.

use super::*;

impl Builder {
    pub fn memory_context(
        &self,
        origin: otto_core::memory::Origin,
        session_id: Option<String>,
    ) -> otto_core::memory::MemoryContext {
        let user = self
            .memory
            .extension
            .as_ref()
            .map(|extension| extension.user_scope.clone())
            .unwrap_or_else(|| neutral_scope(&self.memory.user_scope));
        let workspace = neutral_scope(&self.workspace_scope);
        let organization_id = self
            .memory
            .extension
            .as_ref()
            .and_then(|extension| extension.organization_id.clone());
        let mut scopes = vec![user.clone(), workspace.clone()];
        if let Some(id) = &organization_id {
            scopes.push(otto_core::memory::Scope {
                namespace: "organization".into(),
                id: id.clone(),
            });
        }
        otto_core::memory::MemoryContext {
            organization_id,
            principal: user.id.clone(),
            scopes,
            write_scope: Some(workspace.clone()),
            workspace_id: Some(workspace.id),
            session_id,
            task_id: None,
            origin,
        }
    }

    /// The memory tools, in the order they are registered.
    pub fn memory_tools(
        &self,
        max_output: usize,
        advisor: Option<Arc<crate::memory::advice::Advisor>>,
        turn: Option<(&str, Arc<crate::schedule::tools::TurnOrigin>)>,
    ) -> Vec<Box<dyn Tool + Send + Sync>> {
        if self.memory.extension.is_some() {
            return self
                .memory
                .client
                .as_ref()
                .map(|client| {
                    crate::tool::memory_service::tools(
                        client.clone(),
                        self.memory_context(otto_core::memory::Origin::Model, None),
                        max_output,
                    )
                })
                .unwrap_or_default();
        }
        let scopes = vec![self.memory.user_scope.clone(), self.workspace_scope.clone()];
        let mut remember = RememberTool::new(
            Arc::clone(&self.memory.service),
            self.memory.user_scope.clone(),
            self.workspace_scope.clone(),
        );
        let mut forget = ForgetTool::new(Arc::clone(&self.memory.service), scopes.clone());
        if let Some((session_id, origin)) = turn {
            remember = remember.with_turn_context(session_id, Arc::clone(&origin));
            forget = forget.with_turn_context(session_id, origin);
        }
        vec![
            Box::new(MemorySearchTool::new(
                Arc::clone(&self.memory.service),
                scopes,
                max_output,
            )),
            Box::new(remember.with_advisor(advisor)),
            Box::new(forget),
        ]
    }

    /// The advisor that adds review hints to memory candidates: the auxiliary
    /// Jev model when `TYPESAFE_API_KEY` is non-empty, otherwise the session
    /// model through `agent`, a tool-less agent over the runner's provider.
    /// `None` when memory is unusable.
    pub fn advisor(
        &self,
        agent: super::super::builder::AdviceAgent,
        model: &str,
        usage: Option<crate::usage::Collector>,
    ) -> Option<Arc<crate::memory::advice::Advisor>> {
        if !self.memory.usable || self.memory.extension.is_some() {
            return None;
        }
        let key = self
            .environment
            .get("TYPESAFE_API_KEY")
            .map_or("", String::as_str);
        let backend: Box<dyn crate::decision::Backend> = if key.is_empty() {
            Box::new(super::super::advice::SessionBackend::new(
                agent,
                model.to_owned(),
                usage,
            ))
        } else {
            Box::new(crate::decision::jev::Jev::new(
                crate::typesafe::BASE_URL,
                key,
            ))
        };
        Some(Arc::new(crate::memory::advice::Advisor::new_with_audit(
            backend,
            self.memory.advice_audit.clone(),
        )))
    }

    /// Whether memory tools belong in this build. The boundary half of the
    /// decision is checked by the caller that already knows it.
    pub fn memory_usable(&self) -> bool {
        self.memory.usable
    }
}

impl Builder {
    /// Binds the memory service for one session.
    pub fn bind_memory(&self) -> Result<Binding, BuildError> {
        self.memory
            .service
            .bind(BindOptions {
                scopes: vec![self.memory.user_scope.clone(), self.workspace_scope.clone()],
                default_write_scope: self.workspace_scope.clone(),
                ..BindOptions::default()
            })
            .map_err(|error| format!("bind memory: {error}"))
    }

    pub fn bind_selected_memory(
        &self,
        session_id: String,
    ) -> Result<Arc<dyn otto_core::agent::memory::MemoryRecall + Send + Sync>, BuildError> {
        if self.memory.extension.is_some() {
            let client = self
                .memory
                .client
                .clone()
                .ok_or("memory service unavailable")?;
            Ok(Arc::new(ServiceBinding {
                client,
                context: self.memory_context(otto_core::memory::Origin::Model, Some(session_id)),
                closed: AtomicBool::new(false),
            }))
        } else {
            Ok(Arc::new(self.bind_memory()?))
        }
    }

    /// The memory, skill and sub-agent halves of `boundaryToolDefinitions`. The
    /// caller splices them around the bash definition.
    pub fn boundary_memory_definitions(&self, max_output: usize) -> Vec<ToolDefinition> {
        if !self.memory.usable {
            return Vec::new();
        }
        self.memory_tools(max_output, None, None)
            .iter()
            .map(|tool| tool.definition())
            .collect()
    }

    /// The skill and sub-agent definitions the boundary check must see.
    /// `dynamic` is the redaction-boundary gate on the sub-agent tools.
    pub fn boundary_catalog_definitions(
        &self,
        max_output: usize,
        dynamic: bool,
    ) -> Vec<ToolDefinition> {
        let mut definitions = Vec::new();
        if resolve_skills(&self.config, &self.environment, &self.workspace_path).enabled {
            definitions.push(SkillTool::new(skill::Catalog::default(), max_output).definition());
        }
        // Erring toward including the agent tools keeps the check
        // conservative; `build_catalogs` is the gate that surfaces the error.
        if dynamic
            && resolve_agents(&self.config, &self.environment, &self.workspace_path)
                .map(|agents| agents.enabled)
                .unwrap_or(true)
        {
            definitions.extend(subagent::tools::tool_definitions());
            definitions.extend(remind::definitions());
        }
        definitions
    }
}
