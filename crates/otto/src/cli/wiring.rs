//! Memory, skill and sub-agent wiring for one runner build.
//!
//! Port of Go `cmd/otto/memory_wiring.go` plus the memory, skill and sub-agent
//! blocks of `cmd/otto/runtime_builder.go`'s `buildRunner` and
//! `boundaryToolDefinitions`. The substantive code lives here so the shared
//! composition root only gains call sites.
//!
//! Divergences from Go, each unobservable:
//!
//! - Go asks the store factory for `Capabilities.EncryptionAtRest`. The SQLite
//!   store is the only backend and never encrypts at rest, so
//!   `require_encryption` always fails startup here.
//! - Go creates the memory binding before building the provider client and
//!   releases it on every later failure. Here the binding is created last, so
//!   there is no failure left to unwind.
//! - Go shares one tool slice between the parent registry and the sub-agent
//!   runner. Rust tools are boxed and cannot be cloned, so the children get
//!   their own instances of the same tools, built the same way.

use std::io::Write;
use std::sync::Arc;

use otto_core::agent::inbox::Inbox;
use otto_core::agent::redactor::Redactor;
use otto_core::config::agents::AgentsRuntime;
use otto_core::config::memory::MemoryRuntime;
use otto_core::config::{Runtime, resolve_agents, resolve_skills};
use otto_core::model::ToolDefinition;

use super::runtime_builder::{BuildError, Builder, ProviderClient, SharedSession};
use crate::memory::guard::{CompositeGuard, DefaultGuard, ExactGuard};
use crate::memory::scope::new_workspace_scope;
use crate::memory::sqlite::{Options as StoreOptions, Store};
use crate::memory::{
    BindOptions, Binding, ErrorKind, MAX_EXACT_GUARD_VALUE_BYTES, Scope, Service, new_id,
};
use crate::skill;
use crate::subagent;
use crate::subagent::runner::{
    Config as RunnerConfig, OptionsTemplate, PromptFor, Runner as SubagentRunner,
};
use crate::subagent::tasks::Tasks;
use crate::tool::Tool;
use crate::tool::memory::{ForgetTool, MemorySearchTool, RememberTool};
use crate::tool::skill::SkillTool;

/// The process-wide memory service and the two scopes one session reads.
///
/// The default is Go's zero `runtimeBuilder`: a null service reporting memory
/// as disabled, which every operation answers without touching a store.
pub struct MemoryWiring {
    pub service: Arc<Service>,
    /// Whether the service is backed by a live store, and so worth exposing to
    /// the agent. Go's `memoryUsable`.
    pub usable: bool,
    pub user_scope: Scope,
    pub workspace_scope: Scope,
    pub recall_limit: i64,
    pub recall_token_budget: i64,
}

impl Default for MemoryWiring {
    fn default() -> Self {
        Self {
            service: Arc::new(Service::null(None)),
            usable: false,
            user_scope: Scope::default(),
            workspace_scope: Scope::default(),
            recall_limit: 0,
            recall_token_budget: 0,
        }
    }
}

/// Port of `openMemoryService`. Disabled config, or an open failure when
/// `required` is false, degrades to a null service with a stderr warning
/// rather than failing startup.
pub fn open_memory_service(
    config: &MemoryRuntime,
    secret_values: &[String],
    stderr: &mut dyn Write,
) -> Result<(Arc<Service>, Scope, bool), String> {
    if !config.enabled {
        return Ok((
            Arc::new(Service::null(Some(ErrorKind::Disabled))),
            Scope::default(),
            false,
        ));
    }

    let guard_values: Vec<String> = secret_values
        .iter()
        .filter(|value| value.len() <= MAX_EXACT_GUARD_VALUE_BYTES)
        .cloned()
        .collect();
    let exact = ExactGuard::new(&guard_values)
        .map_err(|error| format!("build memory secret guard: {error}"))?;

    let options = StoreOptions {
        busy_timeout: if config.sqlite_busy_timeout.is_zero() {
            StoreOptions::default().busy_timeout
        } else {
            config.sqlite_busy_timeout
        },
        new_id: Box::new(new_id),
        guard: Box::new(CompositeGuard::new(vec![
            Box::new(DefaultGuard),
            Box::new(exact),
        ])),
    };
    let store = match Store::open(std::path::Path::new(&config.sqlite_path), options) {
        Ok(store) => store,
        Err(error) => {
            if config.required {
                return Err(format!("open memory store: {error}"));
            }
            let _ = writeln!(
                stderr,
                "warning: memory store unavailable, continuing without memory: {error}"
            );
            return Ok((
                Arc::new(Service::null(Some(error.kind))),
                Scope::default(),
                false,
            ));
        }
    };

    // require_encryption is a security requirement, not an availability
    // preference: the SQLite backend never encrypts at rest, so this fails
    // startup regardless of `required`.
    if config.require_encryption {
        let _ = store.close();
        return Err(format!(
            "memory backend {:?} does not support encryption at rest, but require_encryption is set",
            config.backend
        ));
    }

    let identity = match store.identity() {
        Ok(identity) => identity,
        Err(error) => {
            let _ = store.close();
            if config.required {
                return Err(format!("read memory store identity: {error}"));
            }
            let _ = writeln!(
                stderr,
                "warning: memory store unavailable, continuing without memory: {error}"
            );
            return Ok((
                Arc::new(Service::null(Some(error.kind))),
                Scope::default(),
                false,
            ));
        }
    };
    let service = Service::new(store, crate::memory::decide_default_policy);
    Ok((Arc::new(service), identity.user_scope, true))
}

/// Port of `workspaceMemoryScope`: a configured stable id keeps a moved
/// workspace's records reachable.
pub fn workspace_memory_scope(
    config: &MemoryRuntime,
    canonical_path: &str,
) -> Result<Scope, String> {
    let stable = config
        .workspace_ids
        .get(canonical_path)
        .map(String::as_str)
        .unwrap_or_default();
    new_workspace_scope(canonical_path, stable).map_err(|error| error.to_string())
}

/// Configured roots are strings; discovery takes paths.
fn roots(paths: &[String]) -> Vec<std::path::PathBuf> {
    paths.iter().map(std::path::PathBuf::from).collect()
}

/// The skills and agents resolved for one build, plus their prompt sections.
pub struct CatalogWiring {
    pub skills: skill::Catalog,
    pub skill_section: String,
    pub agents: AgentsRuntime,
    pub agent_catalog: subagent::Catalog,
    pub agent_section: String,
}

/// What the sub-agent runner contributes to the parent's agent options.
#[derive(Default)]
pub struct SubagentWiring {
    pub tasks: Option<Arc<Tasks>>,
    pub inbox: Option<Arc<Inbox>>,
}

impl Builder {
    /// The memory tools, in Go's `memoryTools` order.
    pub fn memory_tools(&self, max_output: usize) -> Vec<Box<dyn Tool + Send + Sync>> {
        let scopes = vec![
            self.memory.user_scope.clone(),
            self.memory.workspace_scope.clone(),
        ];
        vec![
            Box::new(MemorySearchTool::new(
                Arc::clone(&self.memory.service),
                scopes.clone(),
                max_output,
            )),
            Box::new(RememberTool::new(
                Arc::clone(&self.memory.service),
                self.memory.workspace_scope.clone(),
            )),
            Box::new(ForgetTool::new(Arc::clone(&self.memory.service), scopes)),
        ]
    }

    /// Whether memory tools belong in this build. Go's `b.memoryUsable &&
    /// b.boundaryAllowsDynamic(&runtime)` at the `buildRunner` call site; the
    /// boundary half is checked by the caller that already knows it.
    pub fn memory_usable(&self) -> bool {
        self.memory.usable
    }

    /// Discovers skills and agent definitions, prints their warnings, and
    /// appends the skill tool to `tools`. Port of the two discovery blocks in
    /// `buildRunner`.
    pub fn build_catalogs(
        &self,
        tools: &mut Vec<Box<dyn Tool + Send + Sync>>,
        max_output: usize,
        stderr: &mut dyn Write,
    ) -> Result<CatalogWiring, BuildError> {
        let skills = resolve_skills(&self.config, &self.environment, &self.workspace_path);
        let (catalog, mut warnings) = skill::Catalog::discover(&roots(&skills.roots));
        let (skill_section, section_warnings) = skill::prompt_section(&catalog);
        warnings.extend(section_warnings);
        for warning in &warnings {
            let _ = writeln!(stderr, "warning: {warning}");
        }
        if !catalog.is_empty() {
            tools.push(Box::new(SkillTool::new(catalog.clone(), max_output)));
        }

        let agents = resolve_agents(&self.config, &self.environment, &self.workspace_path)
            .map_err(|error| error.to_string())?;
        let (agent_catalog, mut agent_warnings) = if agents.enabled {
            subagent::Catalog::discover(&roots(&agents.roots))
        } else {
            (subagent::Catalog::default(), Vec::new())
        };
        let (agent_section, section_warnings) = subagent::prompt_section(&agent_catalog);
        agent_warnings.extend(section_warnings);
        for warning in &agent_warnings {
            let _ = writeln!(stderr, "warning: {warning}");
        }

        Ok(CatalogWiring {
            skills: catalog,
            skill_section,
            agents,
            agent_catalog,
            agent_section,
        })
    }

    /// Builds the sub-agent runner and appends its three tools to `tools`.
    /// Port of `buildRunner`'s `if client != nil && agents.Enabled` block.
    #[allow(clippy::too_many_arguments)]
    pub fn build_subagents(
        &self,
        tools: &mut Vec<Box<dyn Tool + Send + Sync>>,
        catalogs: &CatalogWiring,
        client: &ProviderClient,
        redaction_values: &[String],
        redactor: &Redactor,
        runtime: &Runtime,
        session: &SharedSession,
        prompt_for: PromptFor,
        child_tools: Vec<Box<dyn Tool + Send + Sync>>,
        stderr: &mut dyn Write,
    ) -> Result<SubagentWiring, BuildError> {
        let ProviderClient::Compat(provider) = client else {
            return Ok(SubagentWiring::default());
        };
        if !catalogs.agents.enabled {
            return Ok(SubagentWiring::default());
        }

        let tasks = Arc::new(Tasks::new());
        let session = session.clone();
        let (runner, warnings) = SubagentRunner::new(RunnerConfig {
            provider: Arc::clone(provider) as Arc<dyn otto_core::provider::Provider + Send + Sync>,
            tools: child_tools,
            redaction_values: redaction_values.to_vec(),
            redaction_complete: redactor.allows_dynamic_content(),
            template: OptionsTemplate {
                model: runtime.model.clone(),
                provider_name: runtime.provider.clone(),
                thinking: runtime.thinking.clone(),
                compaction: otto_core::agent::CompactionSettings {
                    auto: runtime.compaction.auto,
                    hard_input_window: runtime.compaction.hard_input_window,
                    working_window: runtime.compaction.working_window,
                    reserve_tokens: runtime.compaction.reserve_tokens,
                    keep_recent_tokens: runtime.compaction.keep_recent_tokens,
                },
                ..OptionsTemplate::default()
            },
            prompt_for,
            tasks: Arc::clone(&tasks),
            catalog: catalogs.agent_catalog.clone(),
            parent_session: Some(Arc::new(move || {
                otto_core::session::Session::messages(&session)
            })),
            max_parallel: catalogs.agents.max_parallel.max(0) as usize,
            max_output_bytes: runtime.max_output_bytes.max(0) as usize,
        })
        .map_err(|error| format!("create sub-agent runner: {error}"))?;
        for warning in &warnings {
            let _ = writeln!(stderr, "warning: {warning}");
        }

        let inbox = Arc::clone(tasks.notifications());
        tools.extend(subagent::tools::tools(&Arc::new(runner)));
        Ok(SubagentWiring {
            tasks: Some(tasks),
            inbox: Some(inbox),
        })
    }

    /// A second set of the parent's non-memory tools, for the children.
    /// Go shares one slice; boxed Rust tools cannot be cloned.
    pub fn child_tools(
        &self,
        runtime: &Runtime,
        max_output: usize,
        redaction_values: &[String],
        skills: &skill::Catalog,
    ) -> Result<Vec<Box<dyn Tool + Send + Sync>>, BuildError> {
        let mut tools = self.builtin_file_tools(max_output);
        if self.bash_configured() {
            let executor = self
                .command_executor
                .clone()
                .expect("bash_configured implies an executor");
            let tool = crate::tool::bash::BashTool::new(
                self.workspace,
                executor,
                &self.shell,
                self.sandbox_environment.clone().unwrap_or_default(),
                super::runtime_builder::shell_timeout(runtime.shell_timeout),
                max_output,
                redaction_values,
            )
            .map_err(|error| format!("create bash tool: {error}"))?;
            tools.push(Box::new(tool));
        }
        if !skills.is_empty() {
            tools.push(Box::new(SkillTool::new(skills.clone(), max_output)));
        }
        Ok(tools)
    }

    /// Renders a child's static system prompt: the parent's prompt for the
    /// child's tool set, plus the shared redacted tail. The Agents section is
    /// parent-only, because children never have the agent tool.
    pub fn child_prompt_for(
        &self,
        runtime: &Runtime,
        endpoint_host: &str,
        prompt_tail: &str,
    ) -> PromptFor {
        let sandbox = self.effective_sandbox_info();
        let provider = runtime.provider.clone();
        let host = endpoint_host.to_string();
        let model = runtime.model.clone();
        let tail = prompt_tail.to_string();
        Arc::new(move |definitions: &[ToolDefinition]| {
            super::prompt::system_prompt_for(definitions, sandbox, &provider, &host, &model) + &tail
        })
    }

    /// Binds the memory service for one session. Port of `buildRunner`'s
    /// `memoryService.Bind`.
    pub fn bind_memory(&self) -> Result<Binding, BuildError> {
        self.memory
            .service
            .bind(BindOptions {
                scopes: vec![
                    self.memory.user_scope.clone(),
                    self.memory.workspace_scope.clone(),
                ],
                default_write_scope: self.memory.workspace_scope.clone(),
                ..BindOptions::default()
            })
            .map_err(|error| format!("bind memory: {error}"))
    }

    /// The memory, skill and sub-agent halves of `boundaryToolDefinitions`.
    /// The caller splices them around the bash definition, in Go's order.
    pub fn boundary_memory_definitions(&self, max_output: usize) -> Vec<ToolDefinition> {
        if !self.memory.usable {
            return Vec::new();
        }
        self.memory_tools(max_output)
            .iter()
            .map(|tool| tool.definition())
            .collect()
    }

    /// The skill and sub-agent definitions the boundary check must see.
    /// `dynamic` is Go's non-recursive `secretRedactor(runtime).
    /// AllowsDynamicContent()` gate on the sub-agent tools.
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
        }
        definitions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{NAMESPACE_USER, NAMESPACE_WORKSPACE, RememberRequest, SearchRequest};

    /// A database path whose parent is a regular file, so every open fails
    /// deterministically. Port of Go's `unopenableMemoryPath`.
    fn unopenable_path(directory: &std::path::Path) -> String {
        let blocker = directory.join("blocker-file");
        std::fs::write(&blocker, b"not a directory").expect("write blocker");
        blocker.join("memory.db").to_string_lossy().into_owned()
    }

    /// A store path one level below the temporary directory, so the store
    /// creates the parent itself at 0700: it rejects a group-readable parent.
    fn store_path(directory: &std::path::Path) -> String {
        directory
            .join("memory")
            .join("memory.db")
            .to_string_lossy()
            .into_owned()
    }

    fn enabled(path: String) -> MemoryRuntime {
        MemoryRuntime {
            enabled: true,
            backend: "sqlite".into(),
            sqlite_path: path,
            ..MemoryRuntime::default()
        }
    }

    #[test]
    fn disabled_memory_returns_an_unusable_null_service() {
        let mut stderr = Vec::new();
        let (service, scope, usable) =
            open_memory_service(&MemoryRuntime::default(), &[], &mut stderr).expect("open");
        assert!(!usable);
        assert_eq!(scope, Scope::default());
        assert_eq!(
            service
                .search(&SearchRequest::default())
                .expect_err("disabled")
                .kind,
            ErrorKind::Disabled
        );
        assert!(stderr.is_empty(), "{stderr:?}");
        service.close().expect("close");
    }

    #[test]
    fn an_open_failure_degrades_to_a_null_service_unless_required() {
        let directory = tempfile::tempdir().expect("directory");
        let mut config = enabled(unopenable_path(directory.path()));

        let mut stderr = Vec::new();
        let (service, scope, usable) =
            open_memory_service(&config, &[], &mut stderr).expect("degrade");
        assert!(!usable);
        assert_eq!(scope, Scope::default());
        assert_eq!(
            service
                .search(&SearchRequest::default())
                .expect_err("unavailable")
                .kind,
            ErrorKind::Unavailable
        );
        assert!(
            String::from_utf8_lossy(&stderr).contains("warning"),
            "{stderr:?}"
        );
        service.close().expect("close");

        config.required = true;
        assert!(open_memory_service(&config, &[], &mut Vec::new()).is_err());
    }

    #[test]
    fn a_successful_open_returns_a_stable_user_scope() {
        let directory = tempfile::tempdir().expect("directory");
        let config = enabled(store_path(directory.path()));

        let (service, scope, usable) =
            open_memory_service(&config, &[], &mut Vec::new()).expect("open");
        assert!(usable);
        assert_eq!(scope.namespace, NAMESPACE_USER);
        assert!(!scope.id.is_empty());
        service.close().expect("close");

        let (reopened, again, usable) =
            open_memory_service(&config, &[], &mut Vec::new()).expect("reopen");
        assert!(usable);
        assert_eq!(again, scope, "the installation id must survive a reopen");
        reopened.close().expect("close");
    }

    #[test]
    fn require_encryption_fails_because_sqlite_never_encrypts_at_rest() {
        let directory = tempfile::tempdir().expect("directory");
        let config = MemoryRuntime {
            require_encryption: true,
            ..enabled(store_path(directory.path()))
        };
        let error = match open_memory_service(&config, &[], &mut Vec::new()) {
            Err(error) => error,
            Ok(_) => panic!("require_encryption must fail startup"),
        };
        assert!(error.contains("encrypt"), "{error}");
    }

    #[test]
    fn configured_secret_values_are_rejected_in_remembered_text() {
        let directory = tempfile::tempdir().expect("directory");
        let config = enabled(store_path(directory.path()));
        let secrets = ["sk-configured-secret".to_string()];

        let (service, scope, usable) =
            open_memory_service(&config, &secrets, &mut Vec::new()).expect("open");
        assert!(usable);
        let error = service
            .remember(&RememberRequest {
                scope,
                kind: "preference".into(),
                text: "the api key is sk-configured-secret".into(),
                ..RememberRequest::default()
            })
            .expect_err("sensitive");
        assert_eq!(error.kind, ErrorKind::SensitiveMemory);
        service.close().expect("close");
    }

    #[test]
    fn an_invalid_secret_value_fails_the_open() {
        let directory = tempfile::tempdir().expect("directory");
        let config = enabled(store_path(directory.path()));
        assert!(
            open_memory_service(&config, &[String::new()], &mut Vec::new()).is_err(),
            "an empty guard value must be rejected"
        );
    }

    #[test]
    fn the_workspace_scope_uses_a_configured_override() {
        let config = MemoryRuntime {
            workspace_ids: std::collections::HashMap::from([(
                "/work/otto".to_string(),
                "custom-id".to_string(),
            )]),
            ..MemoryRuntime::default()
        };
        let scope = workspace_memory_scope(&config, "/work/otto").expect("scope");
        assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);
        assert_eq!(scope.id, "custom-id");
    }

    #[test]
    fn the_workspace_scope_is_derived_from_the_path_without_an_override() {
        let directory = tempfile::tempdir().expect("directory");
        let scope = workspace_memory_scope(
            &MemoryRuntime::default(),
            &directory.path().to_string_lossy(),
        )
        .expect("scope");
        assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);
        assert!(scope.id.starts_with("sha256:"), "{}", scope.id);
    }
}

/// Port of `cmd/otto/skills_wiring_test.go` and `cmd/otto/agents_wiring_test.go`.
///
/// Go drives those cases end to end through `runForTest` against an
/// `httptest` provider, which the offline rule forbids here, so each one is
/// folded onto the composition seam it exercises: [`Builder::build_catalogs`]
/// for the prompt sections, the tool list and the warnings,
/// [`Builder::boundary_catalog_definitions`] for the redaction boundary, and
/// `resolve_sandbox_settings` for the Seatbelt read paths. Go's
/// `TestRunSkillListingAndToolRoundTrip` also asserts a full `skill` tool
/// call/result round trip and the section's position after `## Environment`;
/// the round trip belongs to `skill::tool`'s own tests and the ordering to
/// `system_prompt_for`, so neither is repeated.
#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::cli::testutil;
    use std::path::{Path, PathBuf};

    /// Writes `<root>/<directory>/<file>` with Go's frontmatter shape and
    /// returns the definition directory. Port of `writeSkillFileForTest` and
    /// `writeAgentFileForTest`, which differ only in the file name.
    fn write_definition(
        root: &Path,
        file: &str,
        directory: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> PathBuf {
        let directory = root.join(directory);
        std::fs::create_dir_all(&directory).expect("definition directory");
        let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}");
        std::fs::write(directory.join(file), content).expect("definition file");
        directory
    }

    /// A builder over two canonical temporary directories, with `HOME` set so
    /// the default `~/.otto/...` roots resolve under the first one.
    struct Fixture {
        _home: tempfile::TempDir,
        _workspace: tempfile::TempDir,
        home: PathBuf,
        workspace: PathBuf,
        builder: Builder,
    }

    fn fixture() -> Fixture {
        let home_dir = tempfile::tempdir().expect("home");
        let workspace_dir = tempfile::tempdir().expect("workspace");
        let home = std::fs::canonicalize(home_dir.path()).expect("canonical home");
        let workspace = std::fs::canonicalize(workspace_dir.path()).expect("canonical workspace");
        let mut builder = testutil::builder(&workspace, workspace_dir.path());
        builder
            .environment
            .insert("HOME".to_string(), home.to_string_lossy().into_owned());
        Fixture {
            _home: home_dir,
            _workspace: workspace_dir,
            home,
            workspace,
            builder,
        }
    }

    /// `build_catalogs` with its two output channels made inspectable.
    fn catalogs(builder: &Builder) -> (CatalogWiring, Vec<String>, String) {
        let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
        let mut stderr = Vec::new();
        let wiring = builder
            .build_catalogs(&mut tools, 65536, &mut stderr)
            .expect("build_catalogs");
        let names = tools
            .iter()
            .map(|tool| tool.definition().name.clone())
            .collect();
        (
            wiring,
            names,
            String::from_utf8(stderr).expect("utf-8 warnings"),
        )
    }

    fn definition_names(definitions: &[ToolDefinition]) -> Vec<String> {
        definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect()
    }

    #[test]
    fn a_user_level_skill_is_listed_and_registers_the_skill_tool() {
        let fixture = fixture();
        let directory = write_definition(
            &fixture.home.join(".otto/skills"),
            "SKILL.md",
            "pdf",
            "pdf",
            "Extract text and tables from PDF files.",
            "# PDF handling\nExtract pdfs from files.\n",
        );

        let (wiring, tools, warnings) = catalogs(&fixture.builder);
        assert!(
            wiring.skill_section.contains("\n\n## Skills\n"),
            "{:?}",
            wiring.skill_section
        );
        assert!(
            wiring.skill_section.contains(&format!(
                "<skill name=\"pdf\" location=\"{}\"",
                directory.display()
            )),
            "{:?}",
            wiring.skill_section
        );
        assert_eq!(tools, vec!["skill".to_string()]);
        assert_eq!(warnings, "");
    }

    #[test]
    fn a_workspace_skill_overrides_a_user_level_skill() {
        let fixture = fixture();
        let user = write_definition(
            &fixture.home.join(".otto/skills"),
            "SKILL.md",
            "repo-notes",
            "repo-notes",
            "User-level notes skill.",
            "user body\n",
        );
        let workspace = write_definition(
            &fixture.workspace.join(".otto/skills"),
            "SKILL.md",
            "repo-notes",
            "repo-notes",
            "Workspace-level notes skill.",
            "workspace body\n",
        );

        let (wiring, _, _) = catalogs(&fixture.builder);
        assert!(
            wiring.skill_section.contains(&format!(
                "<skill name=\"repo-notes\" location=\"{}\"",
                workspace.display()
            )),
            "{:?}",
            wiring.skill_section
        );
        assert!(
            !wiring.skill_section.contains(&user.display().to_string()),
            "the overridden user skill must not be listed: {:?}",
            wiring.skill_section
        );
    }

    #[test]
    fn no_skills_omit_the_section_and_the_tool() {
        let fixture = fixture();
        let (wiring, tools, warnings) = catalogs(&fixture.builder);
        assert_eq!(wiring.skill_section, "");
        assert!(tools.is_empty(), "{tools:?}");
        assert_eq!(warnings, "");
    }

    #[test]
    fn disabled_skills_omit_the_section_the_tool_and_the_boundary_definition() {
        let mut fixture = fixture();
        write_definition(
            &fixture.home.join(".otto/skills"),
            "SKILL.md",
            "pdf",
            "pdf",
            "Extract pdfs.",
            "body\n",
        );
        fixture.builder.config.skills.enabled = Some(false);

        let (wiring, tools, _) = catalogs(&fixture.builder);
        assert_eq!(wiring.skill_section, "");
        assert!(!tools.contains(&"skill".to_string()), "{tools:?}");
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
        assert!(!names.contains(&"skill".to_string()), "{names:?}");
    }

    #[test]
    fn an_invalid_skill_warns_and_is_skipped_while_the_valid_one_is_listed() {
        let fixture = fixture();
        let root = fixture.home.join(".otto/skills");
        let pdf = write_definition(&root, "SKILL.md", "pdf", "pdf", "Extract pdfs.", "body\n");
        write_definition(
            &root,
            "SKILL.md",
            "bad",
            "other",
            "Mismatched name.",
            "body\n",
        );

        let (wiring, _, warnings) = catalogs(&fixture.builder);
        assert!(warnings.starts_with("warning: skill "), "{warnings:?}");
        assert!(
            warnings.contains("does not match directory \"bad\""),
            "{warnings:?}"
        );
        assert!(
            wiring.skill_section.contains(&format!(
                "<skill name=\"pdf\" location=\"{}\"",
                pdf.display()
            )),
            "{:?}",
            wiring.skill_section
        );
    }

    #[test]
    fn a_workspace_agent_is_listed_in_the_agents_section() {
        let fixture = fixture();
        write_definition(
            &fixture.workspace.join(".otto/agents"),
            "AGENT.md",
            "reviewer",
            "reviewer",
            "Reviews code for style and correctness.",
            "# Reviewer\nFocus on style.\n",
        );

        let (wiring, _, warnings) = catalogs(&fixture.builder);
        assert!(wiring.agents.enabled);
        assert!(
            wiring.agent_section.contains("\n\n## Agents\n"),
            "{:?}",
            wiring.agent_section
        );
        assert!(
            wiring.agent_section.contains("<agent name=\"reviewer\">"),
            "{:?}",
            wiring.agent_section
        );
        assert_eq!(warnings, "");
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
        for want in ["agent", "agent_wait", "agent_status"] {
            assert!(
                names.contains(&want.to_string()),
                "{names:?} is missing {want}"
            );
        }
    }

    #[test]
    fn disabled_agents_omit_the_section_and_the_boundary_definitions() {
        let mut fixture = fixture();
        write_definition(
            &fixture.workspace.join(".otto/agents"),
            "AGENT.md",
            "reviewer",
            "reviewer",
            "Reviews code for style and correctness.",
            "body\n",
        );
        fixture.builder.config.agents.enabled = Some(false);

        let (wiring, _, _) = catalogs(&fixture.builder);
        assert!(!wiring.agents.enabled);
        assert_eq!(wiring.agent_section, "");
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
        for unwanted in ["agent", "agent_wait", "agent_status"] {
            assert!(
                !names.contains(&unwanted.to_string()),
                "{names:?} still lists {unwanted}"
            );
        }
    }

    /// Go's `TestRuntimeBuilderBoundaryToolDefinitionsIncludesAgentToolsWhenDynamicAllowed`
    /// reaches the second half through a provider secret that exhausts the
    /// redaction marker; here the caller passes the same decision directly.
    #[test]
    fn a_closed_redaction_boundary_drops_the_agent_definitions() {
        let fixture = fixture();
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, false));
        assert_eq!(names, vec!["skill".to_string()]);
    }

    #[test]
    fn an_out_of_range_max_parallel_fails_the_build() {
        let mut fixture = fixture();
        fixture.builder.config.agents.max_parallel = Some(17);
        let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
        let error = match fixture
            .builder
            .build_catalogs(&mut tools, 65536, &mut Vec::new())
        {
            Err(error) => error,
            Ok(_) => panic!("max_parallel = 17 must fail the build"),
        };
        assert_eq!(
            error,
            "[agents].max_parallel must be between 1 and 16, got 17"
        );
    }

    /// Port of `TestRunSkillsSandboxReadPaths` and `TestRunAgentsSandboxReadPaths`,
    /// folded into one table because both roots travel the same code path.
    #[test]
    fn sandbox_read_paths_pick_up_only_existing_enabled_roots() {
        for (name, make_directories, disabled, want) in [
            ("both roots exist", true, false, true),
            ("roots absent", false, false, false),
            ("disabled with roots present", true, true, false),
        ] {
            let fixture = fixture();
            let skills = fixture.workspace.join(".otto/skills");
            let agents = fixture.workspace.join(".otto/agents");
            if make_directories {
                std::fs::create_dir_all(&skills).expect("skills root");
                std::fs::create_dir_all(&agents).expect("agents root");
            }
            let mut config = fixture.builder.config.clone();
            if disabled {
                config.skills.enabled = Some(false);
                config.agents.enabled = Some(false);
            }
            let settings = crate::cli::run::resolve_sandbox_settings(
                &config,
                &fixture.builder.environment,
                &fixture.workspace.to_string_lossy(),
                None,
            )
            .expect("sandbox settings");
            for root in [&skills, &agents] {
                let root = root.to_string_lossy().into_owned();
                assert_eq!(
                    settings.read_paths.contains(&root),
                    want,
                    "{name}: {:?} against {root}",
                    settings.read_paths
                );
            }
        }
    }
}
