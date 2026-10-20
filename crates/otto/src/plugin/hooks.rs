//! Sandboxed lifecycle hooks and custom tools on installed, content-authorized packages.
use crate::hex::to_hex;
use crate::{
    sandbox::{CommandExecutor, Request, Streams},
    tool::workspace::Workspace,
};
use otto_core::{
    agent::hooks::{Decision, Hooks, Response},
    config::plugins::{Components, Hook},
    operation::OperationControl,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
const MAX_INPUT: usize = 1 << 20;
const MAX_OUTPUT: usize = 64 << 10;
#[derive(Clone)]
struct Package {
    installed: super::Installed,
    root: PathBuf,
    data: PathBuf,
    service: super::Service,
}
pub(crate) fn session_guard(
    lease: crate::failover::LeaseSource,
    workspace: PathBuf,
    closed: CancellationToken,
    managed: bool,
) -> Arc<SessionGuard> {
    Arc::new(SessionGuard {
        inner: crate::failover::CommitGuard::new(lease, workspace),
        closed,
        managed,
    })
}
pub struct SessionGuard {
    inner: crate::failover::CommitGuard,
    closed: CancellationToken,
    managed: bool,
}
impl SessionGuard {
    fn before(&self, event: &str) -> Result<(), String> {
        if self.closed.is_cancelled() && (self.managed || event != "session_end") {
            return Err("session ownership ended".into());
        }
        crate::tool::registry::CallGuard::before(&self.inner)
    }
    fn after(&self, event: &str) -> Result<(), String> {
        self.before(event)?;
        crate::tool::registry::CallGuard::after(&self.inner)
    }
}

fn validate_command(command: &[String], timeout: &str) -> Result<Duration, String> {
    if command.is_empty() || command[0].is_empty() || command.iter().any(|s| s.contains('\0')) {
        return Err("extension command requires a nonempty argv".into());
    }
    let value = if timeout.is_empty() {
        Duration::from_secs(10)
    } else {
        Duration::from_nanos(
            u64::try_from(otto_core::config::duration::parse_go_duration(timeout)?)
                .map_err(|_| "extension timeout must be positive")?,
        )
    };
    if value.is_zero() || value > Duration::from_secs(300) {
        return Err("extension timeout must be positive and at most 300 seconds".into());
    }
    Ok(value)
}
fn validate_matcher(event: &str, hook: &Hook) -> Result<(), String> {
    if !hook.matcher.is_empty() && !["before_tool", "after_tool"].contains(&event) {
        return Err("hook matcher is only supported for tool events".into());
    }
    if hook.matcher.iter().any(|m| {
        m.is_empty()
            || m.len() > 64
            || m.bytes().filter(|b| *b == b'*').count() > 1
            || !m
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_*-".contains(&b))
    }) {
        return Err("invalid hook matcher; use an exact name or one wildcard".into());
    }
    Ok(())
}
pub(super) fn validate_components(name: &str, components: &Components) -> Result<(), String> {
    if components.version != 1 {
        return Err("dev.otto extension version must be 1".into());
    }
    for (event, hooks) in components.hooks.entries() {
        for hook in hooks {
            validate_command(&hook.command, &hook.timeout)?;
            validate_matcher(event, hook)?;
        }
    }
    let mut names = std::collections::BTreeSet::new();
    for tool in &components.tools {
        if tool.name.is_empty()
            || !tool
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
            || format!("plugin__{name}__{}", tool.name).len() > 64
            || tool.description.trim().is_empty()
            || tool.description.len() > 1024
            || !tool.input_schema.is_object()
            || !names.insert(&tool.name)
        {
            return Err("invalid or duplicate extension tool".into());
        }
        validate_command(&tool.command, &tool.timeout)?;
        schema(&tool.input_schema, None)?;
    }
    if let Some(memory) = &components.memory {
        validate_command(&memory.command, &memory.timeout)?;
    }
    Ok(())
}
fn fingerprint_file(workspace: &Path, path: &Path) -> Result<String, String> {
    if std::fs::symlink_metadata(path)
        .map_err(|_| "cannot inspect hook source")?
        .is_symlink()
    {
        return Err("hook source must not be a symlink".into());
    }
    let pinned = Workspace::new(workspace).map_err(|_| "cannot pin hook workspace")?;
    let mut source = pinned.open(path).map_err(|_| "cannot pin hook source")?;
    let mut bytes = Vec::new();
    (&mut source)
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read hook source")?;
    if bytes.len() > MAX_INPUT {
        return Err("hook source exceeds 1 MiB".into());
    }
    Ok(to_hex(&Sha256::digest(bytes)))
}

#[derive(Clone)]
pub struct Runtime {
    packages: Vec<Package>,
    hooks: Vec<(String, Hook, Option<Package>)>,
    execution: Execution,
    sources: Vec<(PathBuf, String)>,
}

#[derive(Clone)]
pub struct Execution {
    pub executor: Option<Arc<dyn CommandExecutor>>,
    pub environment: Vec<String>,
    /// Bound `NAME=VALUE` entries per installed package; sent only to that package's commands.
    pub package_environment: std::collections::BTreeMap<String, Vec<(String, String)>>,
    pub workspace: PathBuf,
    pub guard: Option<crate::tool::project_guard::Guard>,
    pub secrets: Vec<String>,
    pub logger: Arc<crate::log::Logger>,
}
impl Runtime {
    pub fn new(
        snapshot: &super::Snapshot,
        home: &Path,
        configured: &otto_core::config::plugins::Hooks,
        execution: Execution,
    ) -> Result<Arc<Self>, String> {
        let service = super::Service::new(home);
        let packages = snapshot
            .packages
            .iter()
            .filter(|p| p.trusted && p.package.otto.has_executables())
            .map(|installed| {
                let root = service.package_path(&installed.digest);
                let package_data_dir = service.ensure_data(&installed.name)?;
                Ok(Package {
                    installed: installed.clone(),
                    root,
                    data: package_data_dir,
                    service: service.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut hooks = Vec::new();
        for (event, entries) in configured.entries() {
            for hook in entries {
                validate_command(&hook.command, &hook.timeout)?;
                validate_matcher(event, hook)?;
                hooks.push((event.into(), hook.clone(), None));
            }
        }
        for package in &packages {
            for (event, entries) in package.installed.package.otto.hooks.entries() {
                for hook in entries {
                    validate_matcher(event, hook)?;
                    hooks.push((event.into(), hook.clone(), Some(package.clone())));
                }
            }
        }
        let sources = crate::config::layered::command_sources(configured, &execution.workspace)
            .into_iter()
            .map(|path| fingerprint_file(&execution.workspace, &path).map(|digest| (path, digest)))
            .collect::<Result<_, _>>()?;
        Ok(Arc::new(Self {
            sources,
            packages,
            hooks,
            execution,
        }))
    }
    fn command_environment(&self, package: Option<&Package>) -> Vec<String> {
        let mut environment = self.execution.environment.clone();
        let Some(bound) =
            package.and_then(|p| self.execution.package_environment.get(&p.installed.name))
        else {
            return environment;
        };
        for (name, value) in bound {
            let prefix = format!("{name}=");
            environment.retain(|entry| !entry.starts_with(&prefix));
            environment.push(format!("{prefix}{value}"));
        }
        environment
    }
    fn valid(&self, package: Option<&Package>) -> bool {
        self.execution.guard.as_ref().is_none_or(|g| g.valid())
            && self.sources.iter().all(|(path, digest)| {
                fingerprint_file(&self.execution.workspace, path).is_ok_and(|d| d == *digest)
            })
            && package.is_none_or(|p| p.service.authorized(&p.installed.name, &p.installed.digest))
    }
    pub fn has_hooks(&self) -> bool {
        !self.hooks.is_empty()
    }
    fn redact_json(&self, text: &str) -> String {
        otto_core::agent::redactor::Redactor::new(&self.execution.secrets)
            .redact_json_strings(text)
            .get()
            .to_owned()
    }
    fn redact(&self, text: &str) -> String {
        crate::tool::result::redact_exact_text(text, &self.execution.secrets, "[REDACTED]")
    }
    async fn command(
        &self,
        command: &[String],
        timeout: &str,
        package: Option<&Package>,
        data: Value,
        control: &dyn OperationControl,
        guard: Option<&Arc<SessionGuard>>,
    ) -> Result<String, CommandError> {
        let event = data.get("event").and_then(Value::as_str).unwrap_or("tool");
        if control.admission_stop_reason().is_some() {
            return Err("hook operation stopped".into());
        }
        if guard.is_some_and(|g| g.before(event).is_err()) {
            return Err("session ownership lost".into());
        }
        if !self.valid(package) {
            return Err(
                "plugin source changed or project trust revoked; restart after review".into(),
            );
        }
        let Some(executor) = &self.execution.executor else {
            return Err("sandbox execution unavailable".into());
        };
        let mut argv = command.to_vec();
        if let Some(package) = package {
            for arg in &mut argv {
                *arg = super::runtime::expand(
                    arg,
                    &package.root,
                    &package.data,
                    package.installed.package.format != "agent-plugins-1.0",
                );
            }
        }
        let mut input = self.redact_json(&data.to_string()).into_bytes();
        input.push(b'\n');
        // An oversized `turn_end` input drops the turn content so the hook still runs.
        if input.len() > MAX_INPUT && data.pointer("/data/turn").is_some() {
            let mut without_turn = data.clone();
            if let Some(event_data) = without_turn.get_mut("data").and_then(Value::as_object_mut) {
                event_data.remove("turn");
                event_data.insert("turn_omitted".into(), "size".into());
            }
            input = self.redact_json(&without_turn.to_string()).into_bytes();
            input.push(b'\n');
        }
        if input.len() > MAX_INPUT {
            return Err("plugin input exceeds 1 MiB".into());
        }
        let timeout = validate_command(command, timeout)
            .map_err(CommandError::from)?
            .min(control.remaining().unwrap_or(Duration::from_secs(300)));
        let token = control.cancellation_token().child_token();
        let mut stdout = crate::tool::result::RedactingCollector::new(
            MAX_OUTPUT,
            &self.execution.secrets,
            "[REDACTED]",
        );
        let mut stderr = crate::tool::result::RedactingCollector::new(
            MAX_OUTPUT,
            &self.execution.secrets,
            "[REDACTED]",
        );
        let status = {
            let run = executor.execute_with_grace(
                Request {
                    argv,
                    stdin: Some(input),
                    dir: self.execution.workspace.clone(),
                    env: self.command_environment(package),
                    report_denials: false,
                },
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &token,
                Duration::from_secs(1),
            );
            tokio::pin!(run);
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    biased;
                    result=&mut run => break result,
                    _=&mut deadline => { token.cancel(); let _=run.await; return Err(CommandError::interrupted("plugin command timed out")); },
                    _=tokio::time::sleep(Duration::from_millis(500)) => if guard.is_some_and(|g|g.before(event).is_err()) || !self.valid(package) {
                        token.cancel();
                        let _=run.await;
                        return Err(CommandError::interrupted("plugin source changed or project trust revoked"));
                    },
                }
            }
        };
        if let Err(error) = status.1 {
            return Err(match error {
                crate::sandbox::Error::Closed
                | crate::sandbox::Error::InvalidRequest
                | crate::sandbox::Error::UnsupportedPolicy
                | crate::sandbox::Error::Unavailable(_)
                | crate::sandbox::Error::EnvironmentUnsafe
                | crate::sandbox::Error::ChildLaunch => {
                    CommandError::from("plugin command unavailable")
                }
                crate::sandbox::Error::Cancelled => {
                    CommandError::interrupted("plugin command cancelled")
                }
                _ => CommandError::started("plugin command failed"),
            });
        }
        if status.0.signaled || status.0.code != 0 {
            return Err(CommandError::started("plugin command failed"));
        }
        if !self.valid(package) {
            return Err(CommandError::started(
                "plugin source changed during execution",
            ));
        }
        if guard.is_some_and(|g| g.after(event).is_err()) {
            return Err(CommandError::started(
                "session ownership changed during hook execution",
            ));
        }
        stdout.flush();
        if stdout.collector().discarded() != 0 {
            return Err(CommandError::started("plugin output exceeds 64 KiB"));
        }
        Ok(String::from_utf8_lossy(stdout.collector().bytes()).into_owned())
    }
    pub fn bind(
        self: &Arc<Self>,
        session: String,
        parent: Option<String>,
        task: String,
        mode: &str,
        guard: Option<Arc<SessionGuard>>,
    ) -> Arc<dyn Hooks> {
        Arc::new(Bound {
            runtime: self.clone(),
            guard,
            context: json!({"session_id":session,"parent_session_id":parent,"task_id":task,"mode":mode}),
        })
    }
    pub fn tools(self: &Arc<Self>) -> Vec<Box<dyn crate::tool::Tool + Send + Sync>> {
        self.packages
            .iter()
            .flat_map(|p| {
                p.installed.package.otto.tools.iter().map(move |t| {
                    Box::new(CommandTool {
                        runtime: self.clone(),
                        package: p.clone(),
                        spec: t.clone(),
                    }) as Box<dyn crate::tool::Tool + Send + Sync>
                })
            })
            .collect()
    }
}

struct CommandError {
    message: String,
    started: bool,
    /// Set where `command` cancels a running process: the outcome of the
    /// operation inside it is unknown.
    interrupted: bool,
}
impl CommandError {
    fn started(message: &str) -> Self {
        Self {
            message: message.into(),
            started: true,
            interrupted: false,
        }
    }
    fn interrupted(message: &str) -> Self {
        Self {
            interrupted: true,
            ..Self::started(message)
        }
    }
}
impl From<&str> for CommandError {
    fn from(message: &str) -> Self {
        Self {
            message: message.into(),
            started: false,
            interrupted: false,
        }
    }
}
impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self {
            message,
            started: false,
            interrupted: false,
        }
    }
}

struct Bound {
    runtime: Arc<Runtime>,
    guard: Option<Arc<SessionGuard>>,
    context: Value,
}
#[async_trait::async_trait]
impl Hooks for Bound {
    async fn run(
        &self,
        event: &str,
        mut data: Value,
        control: &dyn OperationControl,
    ) -> Result<Response, String> {
        let mut combined = Response::default();
        let mut diagnostics = Vec::new();
        for (kind, hook, package) in &self.runtime.hooks {
            if kind != event {
                continue;
            }
            let name = if data.get("tool").and_then(Value::as_str) == Some("mcp_call_tool") {
                data.pointer("/arguments/tool")
                    .and_then(Value::as_str)
                    .unwrap_or("mcp_call_tool")
            } else {
                data.get("tool").and_then(Value::as_str).unwrap_or("")
            };
            if !hook.matcher.is_empty() && !hook.matcher.iter().any(|p| matches(p, name)) {
                continue;
            }
            let result = async {
                let output = self
                    .runtime
                    .command(
                        &hook.command,
                        &hook.timeout,
                        package.as_ref(),
                        json!({
                            "version":1,
                            "event":event,
                            "workspace":self.runtime.execution.workspace,
                            "session":self.context,
                            "data":data
                        }),
                        control,
                        self.guard.as_ref(),
                    )
                    .await
                    .map_err(|error| error.message)?;
                let response: Response = if output.trim().is_empty() {
                    Response::default()
                } else {
                    serde_json::from_str(&self.runtime.redact_json(&output))
                        .map_err(|_| "invalid hook JSON response")?
                };
                if response.arguments.is_some() && event != "before_tool"
                    || response.content.is_some() && event != "after_tool"
                    || response.context.is_some()
                        && !["session_start", "before_prompt", "before_compaction"].contains(&event)
                    || (response.decision.is_some() || response.reason.is_some())
                        && ![
                            "session_start",
                            "before_prompt",
                            "before_tool",
                            "before_compaction",
                        ]
                        .contains(&event)
                {
                    return Err("hook response field is not supported for this event".into());
                }
                Ok(response)
            }
            .await;
            match result {
                Err(message) if event.starts_with("before_") || event == "session_start" => {
                    return Err(message);
                }
                Err(message) => diagnostics.push(message),
                Ok(response) => {
                    if response.decision == Some(Decision::Deny) {
                        return Ok(response);
                    }
                    if let Some(arguments) = response.arguments {
                        data["arguments"] = arguments.clone();
                        combined.arguments = Some(arguments);
                    }
                    if let Some(content) = response.content {
                        data["content"] = content.clone().into();
                        combined.content = Some(content);
                    }
                    if let Some(context) = response.context {
                        combined
                            .context
                            .get_or_insert_default()
                            .push_str(&format!("\n{context}"));
                    }
                }
            }
        }
        if !diagnostics.is_empty() {
            self.runtime.execution.logger.warn(
                "hook_failed",
                &[
                    ("event", event.into()),
                    ("error", self.runtime.redact(&diagnostics.join("; "))),
                ],
            );
            return Err(diagnostics.join("; "));
        }
        Ok(combined)
    }
}
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            !suffix.contains('*')
                && name.starts_with(prefix)
                && name.ends_with(suffix)
                && name.len() >= prefix.len() + suffix.len()
        }
        None => pattern == name,
    }
}

struct CommandTool {
    runtime: Arc<Runtime>,
    package: Package,
    spec: otto_core::config::plugins::Tool,
}
#[async_trait::async_trait]
impl crate::tool::Tool for CommandTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        otto_core::model::RetrySafety::NonIdempotent
    }
    fn definition(&self) -> otto_core::model::ToolDefinition {
        crate::tool::definition(
            &format!(
                "plugin__{}__{}",
                self.package.installed.name.replace('.', "_"),
                self.spec.name
            ),
            &self.spec.description,
            self.spec.input_schema.clone(),
        )
    }
    fn approval_spec(
        &self,
        arguments: &serde_json::value::RawValue,
    ) -> Result<Option<crate::tool::ApprovalSpec>, otto_core::tool::ToolResult> {
        if !self.runtime.valid(Some(&self.package)) {
            return Err(otto_core::tool::ToolResult::error(
                "Plugin authorization revoked or content changed",
            )
            .not_started());
        }
        let value: Value = serde_json::from_str(arguments.get()).map_err(|_| {
            otto_core::tool::ToolResult::error("invalid plugin tool arguments").not_started()
        })?;
        schema(&self.spec.input_schema, Some(&value))
            .map_err(|e| otto_core::tool::ToolResult::error(e).not_started())?;
        Ok(Some(crate::tool::ApprovalSpec {
            source: "plugin".into(),
            authority: otto_core::approval::Authority::Bounded,
            version: self.package.installed.digest.clone(),
            title: format!("Run {}", self.definition().name),
            details:
                json!({"tool":self.definition().name,"arguments":value,"command":self.spec.command})
                    .to_string(),
            reason: "Execute a plugin command in the current sandbox".into(),
            persistent: false,
        }))
    }
    async fn execute(
        &self,
        arguments: &serde_json::value::RawValue,
        cancel: &CancellationToken,
    ) -> otto_core::tool::ToolResult {
        self.invoke(arguments, cancel, json!({})).await
    }
    async fn execute_call(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        cancel: &CancellationToken,
    ) -> otto_core::tool::ToolResult {
        self.invoke(call.arguments,cancel,json!({"tool_call_id":call.tool_call_id,"operation_id":call.operation_id,"attempt":call.attempt})).await
    }
    async fn execute_approved(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        cancel: &CancellationToken,
        request: &otto_core::approval::Request,
    ) -> otto_core::tool::ToolResult {
        self.invoke(
            call.arguments,
            cancel,
            json!({
                "tool_call_id":call.tool_call_id,
                "operation_id":call.operation_id,
                "attempt":call.attempt,
                "session_id":request.scope.session_id,
                "task_id":request.scope.task_id,
                "workspace":request.scope.workspace
            }),
        )
        .await
    }
}

impl CommandTool {
    async fn invoke(
        &self,
        arguments: &serde_json::value::RawValue,
        cancel: &CancellationToken,
        metadata: Value,
    ) -> otto_core::tool::ToolResult {
        if let Err(result) = <Self as crate::tool::Tool>::approval_spec(self, arguments) {
            return result;
        }
        let value: Value = serde_json::from_str(arguments.get()).expect("validated JSON");
        match self
            .runtime
            .command(
                &self.spec.command,
                &self.spec.timeout,
                Some(&self.package),
                json!({
                    "version":1,
                    "event":"tool",
                    "workspace":self.runtime.execution.workspace,
                    "tool":<Self as crate::tool::Tool>::definition(self).name,
                    "arguments":value,
                    "metadata":metadata
                }),
                cancel,
                None,
            )
            .await
        {
            Ok(output) => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Result {
                    content: String,
                    #[serde(default)]
                    is_error: bool,
                }
                match serde_json::from_str::<Result>(&self.runtime.redact_json(&output)) {
                    Ok(result) => otto_core::tool::ToolResult {
                        content: result.content,
                        is_error: result.is_error,
                        ..Default::default()
                    },
                    Err(_) => {
                        otto_core::tool::ToolResult::error("invalid plugin tool JSON response")
                            .transport_unknown()
                    }
                }
            }
            Err(error) => {
                if error.started {
                    otto_core::tool::ToolResult::error(error.message).transport_unknown()
                } else {
                    otto_core::tool::ToolResult::error(error.message).not_started()
                }
            }
        }
    }
}

/// Use the standard validator with all network/filesystem reference resolution disabled.
fn schema(spec: &Value, value: Option<&Value>) -> Result<(), String> {
    let validator = jsonschema::options()
        .build(spec)
        .map_err(|_| "invalid or externally referenced plugin tool schema")?;
    if let Some(value) = value {
        validator
            .validate(value)
            .map_err(|_| "plugin tool arguments do not match input_schema")?;
    }
    Ok(())
}

mod memory_source;
pub use memory_source::PluginMemoryService;

#[cfg(test)]
mod memory_source_tests;
#[cfg(test)]
mod tests;
