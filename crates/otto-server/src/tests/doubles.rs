//! Test doubles: scripted provider, elevated bash tool, workspace registry and server factory.

use super::*;

// ---- the provider double ----

/// What one turn does. `Runner` is a concrete struct here, so the script
/// sits one layer down, at the provider.
#[derive(Default)]
pub(super) struct Script {
    /// Streamed as text deltas and returned as the assistant text.
    pub(super) deltas: Vec<String>,
    /// The assistant message's usage.
    pub(super) usage: Option<Usage>,
    /// Fails the call with this message instead of answering.
    pub(super) error: Option<String>,
    /// Streams back the last user message instead of `deltas`.
    pub(super) echo: bool,
    /// Held after the deltas until cancelled, so the turn stays running.
    /// Cancelling the turn wins the race and yields `Cancelled`.
    pub(super) gate: Option<CancellationToken>,
    /// Provider calls numbered up to this (1-based) skip the gate, so earlier
    /// turns finish while a later call (a compaction) is held.
    pub(super) gate_after_calls: usize,
    /// Runs at the top of every call with the 1-based call index. The wake
    /// tests push a notification from it.
    pub(super) on_call: Option<Box<dyn Fn(usize) + Send + Sync>>,
    /// Answers every call whose last request message is not a tool
    /// result with a `bash` tool call for this command; the call after a
    /// tool result answers with `deltas`. Needs [`HarnessOptions::elevate`]
    /// to register the `bash` tool.
    pub(super) tool_command: Option<String>,
    /// Like `tool_command`, for an `update_plan` call with these
    /// arguments; the harness registers the real tool.
    pub(super) plan_call: Option<String>,
}

pub(super) struct ScriptedProvider {
    pub(super) script: Script,
    pub(super) started: watch::Sender<usize>,
    /// The role of each call's last request message, in call order. A
    /// prompt turn ends in [`Role::User`]; a wake turn ends in the
    /// delivered notification's [`Role::Context`].
    pub(super) roles: Mutex<Vec<Role>>,
    /// The last user message of each call, in call order.
    pub(super) texts: Mutex<Vec<String>>,
    /// System prompt of each call, in call order.
    pub(super) system_prompts: Mutex<Vec<String>>,
}

impl ScriptedProvider {
    pub(super) fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            started: watch::channel(0).0,
            roles: Mutex::new(Vec::new()),
            texts: Mutex::new(Vec::new()),
            system_prompts: Mutex::new(Vec::new()),
        })
    }

    pub(super) fn texts(&self) -> Vec<String> {
        self.texts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(super) fn roles(&self) -> Vec<Role> {
        self.roles
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(super) fn system_prompts(&self) -> Vec<String> {
        self.system_prompts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Resolves once `count` calls have reached the gate.
    pub(super) async fn wait_started(&self, count: usize) {
        let mut rx = self.started.subscribe();
        rx.wait_for(|seen| *seen >= count).await.expect("sender");
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: &ProviderRequest,
        emit: StreamSink<'_>,
        control: &dyn otto_core::operation::OperationControl,
    ) -> otto_core::provider::ProviderSettlement {
        let cancel = control.cancellation_token();
        let call = {
            let mut roles = self
                .roles
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            roles.push(
                request
                    .messages
                    .last()
                    .map_or(Role::User, |last| last.role.clone()),
            );
            roles.len()
        };
        self.texts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(
                request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::User)
                    .map(Message::text)
                    .unwrap_or_default(),
            );
        self.system_prompts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(request.system_prompt.clone());
        let after_tool_result = request
            .messages
            .last()
            .is_some_and(|last| last.role == Role::Tool);
        if let Some(hook) = &self.script.on_call {
            hook(call);
        }
        let deltas: Vec<String> = if self.script.echo {
            vec![
                request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::User)
                    .map(Message::text)
                    .unwrap_or_default(),
            ]
        } else {
            self.script.deltas.clone()
        };
        for delta in &deltas {
            emit(StreamEvent::TextDelta {
                text: delta.clone(),
            });
        }
        self.started.send_modify(|seen| *seen += 1);

        if let Some(gate) = self
            .script
            .gate
            .as_ref()
            .filter(|_| call > self.script.gate_after_calls)
        {
            tokio::select! {
                () = gate.cancelled() => {}
                () = cancel.cancelled() => {
                    let reason = control.stop_reason().unwrap_or(
                        otto_core::model::OperationStopReason::UserCancellation,
                    );
                    return otto_core::provider::ProviderSettlement::stopped(
                        if reason == otto_core::model::OperationStopReason::Deadline {
                            ProviderError::DeadlineExceeded
                        } else {
                            ProviderError::Cancelled
                        },
                        0,
                        otto_core::model::EffectCertainty::NotStarted,
                        reason,
                    );
                },
            }
        }
        if let Some(message) = &self.script.error {
            return otto_core::provider::ProviderSettlement::failed(
                ProviderError::Other(message.clone()),
                0,
                otto_core::model::EffectCertainty::Completed,
            );
        }
        let (blocks, finish_reason) = match &self.script.tool_command {
            _ if self.script.plan_call.is_some() && !after_tool_result => (
                vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: format!("call-{call}"),
                    tool_name: "update_plan".to_string(),
                    arguments: Some(
                        serde_json::value::RawValue::from_string(
                            self.script.plan_call.clone().unwrap_or_default(),
                        )
                        .expect("arguments"),
                    ),
                    ..Block::default()
                }],
                Some(otto_core::model::FinishReason::ToolCalls),
            ),
            Some(command) if !after_tool_result => (
                vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: format!("call-{call}"),
                    tool_name: "bash".to_string(),
                    arguments: Some(
                        serde_json::value::RawValue::from_string(
                            serde_json::json!({ "command": command }).to_string(),
                        )
                        .expect("arguments"),
                    ),
                    ..Block::default()
                }],
                Some(otto_core::model::FinishReason::ToolCalls),
            ),
            _ => (
                vec![Block {
                    block_type: BlockType::Text,
                    text: deltas.concat(),
                    ..Block::default()
                }],
                Some(otto_core::model::FinishReason::Stop),
            ),
        };
        let cached_input_tokens_present = self.script.usage.is_some();
        otto_core::provider::ProviderSettlement::succeeded(
            ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    blocks,
                    usage: self.script.usage,
                    finish_reason,
                    ..Message::default()
                },
                cached_input_tokens_present,
                remote_compaction: None,
            },
            0,
        )
    }
}

/// A `bash` tool that asks for approval until the command is granted,
/// then answers `elevated ok`. The request text matches the real tool's,
/// so `parse_approval_request` reads it.
pub(super) struct ElevatedBash {
    pub(super) approvals: Arc<otto::tool::bash::BashApprovals>,
    pub(super) session_id: String,
}

#[async_trait::async_trait]
impl otto::tool::Tool for ElevatedBash {
    fn definition(&self) -> otto_core::model::ToolDefinition {
        otto::tool::definition("bash", "test double", serde_json::json!({"type": "object"}))
    }

    async fn execute(
        &self,
        arguments: &serde_json::value::RawValue,
        _cancel: &CancellationToken,
    ) -> otto_core::tool::ToolResult {
        let command = serde_json::from_str::<Value>(arguments.get())
            .ok()
            .and_then(|value| value["command"].as_str().map(str::to_string))
            .unwrap_or_default();
        if self.approvals.take(&self.session_id, &command) {
            return otto_core::tool::ToolResult {
                content: "elevated ok".to_string(),
                ..Default::default()
            };
        }
        let id = self.approvals.request(&self.session_id, &command);
        otto_core::tool::ToolResult::error(format!(
            "approval required\nApprove in Otto: /approve {id}\nCommand: {}\nJustification: \"needed\"",
            serde_json::to_string(&command).expect("command")
        ))
    }
}

// ---- the workspace registry double ----

/// A fake, in-memory workspace registry for `/v1/workspaces` tests. Real
/// admission and the real load-once registry mutex live in
/// `otto_cli::serve::Workspaces` and its own tests; this only exercises the
/// `Factory`/route wiring, so admission here is a plain allowlist rather
/// than a filesystem check.
pub(super) struct FakeWorkspaces {
    pub(super) startup: String,
    /// Paths besides `startup` this fake admits.
    pub(super) admitted: Vec<String>,
    /// A path that fails to load, with the message to fail it with.
    /// Checked before the path is registered, so a repeat call for the
    /// same path after this fires once can still succeed.
    pub(super) error: Option<(String, String)>,
    /// Held before a load registers its path, so a test can pile up
    /// concurrent loads of one path.
    pub(super) gate: Option<CancellationToken>,
    pub(super) loaded: tokio::sync::Mutex<BTreeMap<String, bool>>,
    /// A loaded path with a simulated active workflow run, so
    /// `remove_workspace` answers `WorkspaceRemoveError::InUse` for it.
    pub(super) in_use: Option<String>,
    /// Paths `trust_workspace` recorded; admitted like `admitted`.
    pub(super) trusted: Mutex<Vec<String>>,
    /// `Factory::browse_roots`'s answer.
    pub(super) browse_roots: Vec<std::path::PathBuf>,
}

impl Default for FakeWorkspaces {
    fn default() -> Self {
        Self {
            startup: String::new(),
            admitted: Vec::new(),
            error: None,
            gate: None,
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
            in_use: None,
            trusted: Mutex::new(Vec::new()),
            browse_roots: Vec::new(),
        }
    }
}

// ---- the factory double ----

pub(super) struct TestFactory {
    pub(super) builder: Arc<Builder>,
    pub(super) provider: Arc<ScriptedProvider>,
    /// Gives every controller this id; otherwise each gets a random one.
    pub(super) fixed_id: Option<String>,
    pub(super) create_error: Option<String>,
    pub(super) open_error: Option<String>,
    /// Held before `open` returns, so a test can pile up concurrent
    /// resumes of one id.
    pub(super) open_gate: Option<CancellationToken>,
    /// Every controller shares this registry. `None` gives each its own.
    pub(super) tasks: Option<Arc<otto::subagent::tasks::Tasks>>,
    pub(super) list: Option<ListResult>,
    /// Backs `Factory::tasks_list`/`tasks_get`. `None` keeps the trait's
    /// empty defaults, matching a process with no recorder wired.
    pub(super) task_recorder: Option<Arc<otto::subagent::record::Store>>,
    pub(super) workspaces: FakeWorkspaces,
    /// Canned `Factory::list` answer for a named workspace (`?workspace=`).
    /// An absent key falls back to `list`.
    pub(super) list_by_workspace: HashMap<String, ListResult>,
    /// Workflow controllers keyed by workspace path. The entry at the
    /// harness's own workspace path is the startup controller.
    pub(super) workflows: HashMap<String, Arc<otto::workflow::Controller>>,
    /// Canned `Factory::reload_sandbox` answer. `None` matches the
    /// trait's default (`sandbox_reload_available` false, `POST
    /// /v1/sandbox/reload` answers 501).
    pub(super) sandbox_reload: Option<Result<SandboxInfo, String>>,
    /// Canned `Factory::reload_other_sandboxes` answer, one entry per
    /// non-startup workspace with its own reloader.
    pub(super) sandbox_reload_others: Vec<(String, Result<SandboxInfo, String>)>,
    /// Canned `Factory::diff_runner` answer for the startup workspace.
    /// `None` matches a workspace with no usable sandbox (`GET
    /// /v1/workspaces/diff` answers 501).
    pub(super) diff_runner: Option<(Arc<dyn otto::sandbox::CommandExecutor>, Vec<String>)>,
    pub(super) persistent: bool,
    pub(super) create_calls: AtomicUsize,
    pub(super) open_calls: AtomicUsize,
}

impl TestFactory {
    /// `workspace` overrides the session header's workspace, matching a
    /// real `Factory::create`/`open` given a non-startup workspace; every
    /// other test-double detail (provider, tasks) stays on the one shared
    /// builder.
    pub(super) fn controller_for(&self, id: &str, workspace: Option<&str>) -> Controller {
        let header = Header {
            version: CURRENT_VERSION,
            id: id.to_string(),
            workspace: workspace
                .unwrap_or(&self.builder.workspace_path)
                .to_string(),
            provider: "openai-compatible".to_string(),
            profile: "alpha".to_string(),
            model: "test-model".to_string(),
            created_at: chrono::Utc::now(),
        };
        let session = if self.persistent {
            // A session file per id under the builder's session root:
            // an existing one (a fork) is opened, a new one created.
            let root = std::path::Path::new(&self.builder.session_root);
            let path = otto::session::session_directory(root, &header.workspace)
                .expect("session directory")
                .join(format!("{id}.jsonl"));
            let store = if path.exists() {
                otto::session::Store::open(&path).expect("open session").0
            } else {
                otto::session::Store::create(root, header).expect("create session")
            };
            SharedSession::new(Arc::new(store))
        } else {
            SharedSession::memory(header)
        };
        let mut tools: Vec<Box<dyn otto::tool::Tool + Send + Sync>> =
            match &self.builder.bash_approvals {
                Some(approvals) => vec![Box::new(ElevatedBash {
                    approvals: Arc::clone(approvals),
                    session_id: id.to_string(),
                })],
                None => Vec::new(),
            };
        if self.provider.script.plan_call.is_some() {
            tools.push(Box::new(otto::tool::plan::UpdatePlan::new(
                Arc::new(session.clone()),
                &[],
            )));
        }
        let mut runner = Runner::scripted_with_tools(
            session.clone(),
            Arc::clone(&self.provider) as Arc<dyn Provider + Send + Sync>,
            self.tasks
                .clone()
                .unwrap_or_else(|| Arc::new(otto::subagent::tasks::Tasks::new())),
            tools,
        );
        if self.builder.config.skills.paths.is_some() {
            let catalogs = self
                .builder
                .build_catalogs(&mut Vec::new(), 4096, &mut Vec::new())
                .expect("skill catalogs");
            runner.install_skill_catalogs(catalogs.skills, catalogs.discovered_skills);
        }
        if self.persistent
            && let Some(journal) =
                self.builder
                    .session_journal(&session, "turns.json", &mut std::io::sink())
        {
            runner.task_control().set_turn_queue_journal(journal);
        }
        Controller::with_builder(
            Arc::clone(&self.builder),
            true,
            session,
            runner,
            RuntimeInfo {
                provider: "alpha".to_string(),
                provider_type: "openai-compatible".to_string(),
                model_selector: "alpha/test-model".to_string(),
                model: "test-model".to_string(),
                thinking: "high".to_string(),
                context_window: 128_000,
                sandbox: self.builder.sandbox_info,
            },
        )
    }
}

#[async_trait::async_trait]
impl Factory for TestFactory {
    fn approval_service(&self) -> Option<Arc<otto::approval::Service>> {
        Some(self.builder.approvals.clone())
    }
    async fn create(&self, workspace: Option<&str>) -> Result<Controller, String> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.create_error {
            return Err(error.clone());
        }
        let id = self
            .fixed_id
            .clone()
            .unwrap_or_else(|| new_id().expect("id"));
        Ok(self.controller_for(&id, workspace))
    }

    async fn create_with_receipt(
        &self,
        workspace: Option<&str>,
        id: String,
        request_id: &str,
    ) -> Result<Controller, String> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        let ctrl = self.controller_for(&id, workspace);
        ctrl.write_session_command_receipt(request_id).await?;
        Ok(ctrl)
    }

    async fn open(&self, id: &str, workspace: Option<&str>) -> Result<Controller, String> {
        self.open_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.open_gate {
            gate.cancelled().await;
        }
        if let Some(error) = &self.open_error {
            return Err(error.clone());
        }
        if self.persistent {
            let path = otto::session::session_directory(
                std::path::Path::new(&self.builder.session_root),
                workspace.unwrap_or(&self.builder.workspace_path),
            )
            .map_err(|error| error.to_string())?
            .join(format!("{id}.jsonl"));
            if !path.is_file() {
                return Err(SESSION_NOT_FOUND.into());
            }
        }
        Ok(self.controller_for(id, workspace))
    }

    async fn list(&self, workspace: Option<&str>) -> Option<Result<ListResult, String>> {
        match workspace {
            Some(path) => self
                .list_by_workspace
                .get(path)
                .cloned()
                .map(Ok)
                .or_else(|| self.list.clone().map(Ok)),
            None => self.list.clone().map(Ok),
        }
    }

    fn tasks_list(
        &self,
        query: &otto::subagent::record::ListQuery,
    ) -> Result<otto::subagent::record::ListResult, String> {
        match &self.task_recorder {
            Some(store) => store.list(query).map_err(|error| error.to_string()),
            None => Ok(otto::subagent::record::ListResult::default()),
        }
    }

    fn tasks_get(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<otto::subagent::record::TaskRow>, String> {
        match &self.task_recorder {
            Some(store) => store
                .get(parent_session, task_id)
                .map_err(|error| error.to_string()),
            None => Ok(None),
        }
    }

    async fn workspaces(&self) -> WorkspaceList {
        let loaded = self.workspaces.loaded.lock().await;
        let mut rest: Vec<WorkspaceInfo> = loaded
            .iter()
            .filter(|(path, _)| **path != self.workspaces.startup)
            .map(|(path, workflows)| WorkspaceInfo {
                path: path.clone(),
                workflows: *workflows,
            })
            .collect();
        rest.sort_by(|a, b| a.path.cmp(&b.path));
        let mut all = vec![WorkspaceInfo {
            path: self.workspaces.startup.clone(),
            workflows: *loaded.get(&self.workspaces.startup).unwrap_or(&false),
        }];
        all.extend(rest);
        WorkspaceList {
            startup: self.workspaces.startup.clone(),
            roots: self.workspaces.admitted.clone(),
            loaded: all,
        }
    }

    async fn load_workspace(
        &self,
        path: &str,
    ) -> Result<(WorkspaceInfo, bool), WorkspaceLoadError> {
        if !path.starts_with('/') {
            return Err(WorkspaceLoadError::Invalid(format!(
                "{path}: not an existing directory"
            )));
        }
        if path != self.workspaces.startup
            && !self.workspaces.admitted.contains(&path.to_string())
            && !self
                .workspaces
                .trusted
                .lock()
                .expect("trusted")
                .contains(&path.to_string())
        {
            return Err(WorkspaceLoadError::NotAdmitted(format!(
                "{path}: outside the startup workspace and configured roots"
            )));
        }
        let mut loaded = self.workspaces.loaded.lock().await;
        if let Some(workflows) = loaded.get(path) {
            return Ok((
                WorkspaceInfo {
                    path: path.to_string(),
                    workflows: *workflows,
                },
                false,
            ));
        }
        if let Some((error_path, message)) = &self.workspaces.error
            && error_path == path
        {
            return Err(WorkspaceLoadError::Failed(message.clone()));
        }
        if let Some(gate) = &self.workspaces.gate {
            gate.cancelled().await;
        }
        loaded.insert(path.to_string(), true);
        Ok((
            WorkspaceInfo {
                path: path.to_string(),
                workflows: true,
            },
            true,
        ))
    }

    async fn trust_workspace(&self, path: &str) -> Result<(), WorkspaceLoadError> {
        if !path.starts_with('/') {
            return Err(WorkspaceLoadError::Invalid(format!(
                "{path}: not an existing directory"
            )));
        }
        self.workspaces
            .trusted
            .lock()
            .expect("trusted")
            .push(path.to_string());
        Ok(())
    }

    fn browse_roots(&self) -> Vec<std::path::PathBuf> {
        self.workspaces.browse_roots.clone()
    }

    async fn remove_workspace(&self, path: &str) -> Result<(), WorkspaceRemoveError> {
        if path == self.workspaces.startup {
            return Err(WorkspaceRemoveError::IsStartup);
        }
        if self.workspaces.in_use.as_deref() == Some(path) {
            return Err(WorkspaceRemoveError::InUse(
                "an active workflow run".to_string(),
            ));
        }
        let mut loaded = self.workspaces.loaded.lock().await;
        if loaded.remove(path).is_none() {
            return Err(WorkspaceRemoveError::NotFound);
        }
        Ok(())
    }

    async fn workflow_controller(
        &self,
        workspace: Option<&str>,
    ) -> Option<Arc<otto::workflow::Controller>> {
        let path = workspace.unwrap_or(&self.builder.workspace_path);
        self.workflows.get(path).cloned()
    }

    async fn workflow_controllers(&self) -> Vec<Arc<otto::workflow::Controller>> {
        let startup = &self.builder.workspace_path;
        let mut rest: Vec<(&String, &Arc<otto::workflow::Controller>)> = self
            .workflows
            .iter()
            .filter(|(path, _)| *path != startup)
            .collect();
        rest.sort_by(|a, b| a.0.cmp(b.0));
        let mut all = Vec::with_capacity(self.workflows.len());
        if let Some(controller) = self.workflows.get(startup) {
            all.push(Arc::clone(controller));
        }
        all.extend(
            rest.into_iter()
                .map(|(_, controller)| Arc::clone(controller)),
        );
        all
    }

    fn sandbox_reload_available(&self) -> bool {
        self.sandbox_reload.is_some()
    }

    async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
        self.sandbox_reload.clone()
    }

    async fn reload_other_sandboxes(&self) -> Vec<(String, Result<SandboxInfo, String>)> {
        self.sandbox_reload_others.clone()
    }

    async fn diff_runner(
        &self,
        workspace: &str,
    ) -> Option<(Arc<dyn otto::sandbox::CommandExecutor>, Vec<String>)> {
        if workspace == self.workspaces.startup {
            self.diff_runner.clone()
        } else {
            None
        }
    }
}
