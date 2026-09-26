//! The sub-agent runner: it starts child agent loops, tracks them in the
//! session's task registry, and renders their completion text.
//!
//! Ownership: a [`Runner`] owns the child tool registry it builds from
//! [`Config::tools`]. The task registry, the provider and the clock are shared
//! with the parent through `Arc`. Each child owns its own transcript: a session
//! file beside the parent's when [`Config::child_session`] returns one, and an
//! in-memory session otherwise.
//!
//! Concurrency and cancellation: every child runs in its own Tokio task,
//! admitted by a semaphore that caps [`Config::max_parallel`] concurrent
//! children. Each task's [`CancellationToken`] is held by the registry, so
//! cancelling it either releases a queued child before it starts or stops a
//! running one mid-turn. [`Runner::start`] itself never blocks.
//!
//! Security: a child never receives the agent-control or memory tools, so it
//! can neither start children of its own nor read or write long-term memory. It
//! also never receives `remind`, which wakes the parent session. A definition's
//! `tools` list can only narrow the set the runner already built, never widen
//! it. Definition bodies are untrusted text, appended to the child's system
//! prompt under a fixed `## Sub-agent role` heading.
//!
//! Shapes forced by the ownership contracts:
//!
//! - [`Options`] holds boxed closures and is not `Clone`, so
//!   [`OptionsTemplate`] carries the copyable settings plus the shared clock
//!   and id generator, and each child builds its own `Options` from it.
//! - The provider sits behind [`SharedProvider`], because [`Agent`] owns its
//!   provider.
//! - [`Registry`] owns boxed tools and cannot be subset, so a child gets a
//!   [`ChildTools`] view over one shared child registry plus an allowlist.
//! - A persisted child transcript is named after its task id, which exists
//!   only after `Tasks::add`, so the transcript is chosen after the task is
//!   added and the registry's history hook reads it through a `OnceLock`. A
//!   transcript that cannot be created fails only that task.
//! - `Session::append` is async, so an inherited snapshot is replayed at the
//!   top of the child task rather than inside `start`; an invalid snapshot
//!   still fails the task with the same message, just asynchronously.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use otto_core::agent::inbox::{Inbox, Notification, NotificationKind};
use otto_core::agent::redactor::Redactor;
use otto_core::agent::{Agent, CompactionSettings, Event, EventSink, Options};
use otto_core::model::{Message, Role, ToolDefinition};
use otto_core::provider::{Provider, ProviderError, Request, RequestSizer, Response, StreamSink};
use otto_core::session::{MemorySession, Session};
use otto_core::tool::{ToolExecutor, ToolResult};
use serde_json::value::RawValue;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::format::{comma_int, first_runes, one_line, round_to_seconds};
use super::tasks::{Task, TaskError, TaskStatus, Tasks};
use super::{Catalog, Definition, WritePolicy, inherit_snapshot};
use crate::tool::Tool;
use crate::tool::registry::Registry;
use crate::tool::result::capped_text_result;

/// Tools a child never receives: the agent-control tools, because delegation
/// depth is fixed at one; the memory tools, because a child gets no memory
/// binding; and the timer tools, which read and wake the parent session.
pub const EXCLUDED_CHILD_TOOLS: [&str; 11] = [
    "agent",
    "agent_wait",
    "agent_status",
    "agent_send",
    "agent_cancel",
    "remember",
    "forget",
    "memory_search",
    "remind",
    "remind_status",
    "remind_cancel",
];

/// Appended to a child's system prompt under `## Sub-agent role` when it has
/// no definition, or its definition's body is empty.
const GENERIC_SUBAGENT_INSTRUCTION: &str = "You are running as a sub-agent of Otto. Complete only the delegated task below with the available tools, then reply with a self-contained final report. That final message is returned to the caller as your result; nothing else you write is.";

const DEFAULT_MAX_PARALLEL: usize = 4;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16384;
const MAX_DESCRIPTION_CHARS: usize = 80;

/// Shares one provider between the parent and every child, because [`Agent`]
/// owns the provider it calls.
pub struct SharedProvider(Arc<dyn Provider + Send + Sync>);

#[async_trait::async_trait]
impl Provider for SharedProvider {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Response, ProviderError> {
        self.0.complete(request, emit, cancel).await
    }
}

/// Shares one child transcript between the child's agent and the runner, which
/// reads it back to build the final report.
pub type Transcript = Arc<dyn Session + Send + Sync>;

/// Builds the transcript for the task with the given id and returns it with
/// its file path. `Ok(None)` keeps the child in memory; an error fails the
/// task before it starts.
pub type ChildSession =
    Arc<dyn Fn(&str) -> Result<Option<(Transcript, String)>, String> + Send + Sync>;

struct SharedTranscript(Transcript);

#[async_trait::async_trait]
impl Session for SharedTranscript {
    fn messages(&self) -> Vec<Message> {
        self.0.messages()
    }

    async fn append(&self, message: Message) -> Result<(), otto_core::session::SessionError> {
        self.0.append(message).await
    }

    fn latest_compaction(&self) -> Option<otto_core::session::CompactionMetadata> {
        self.0.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: otto_core::session::CompactionCheckpoint,
    ) -> Result<otto_core::session::CompactionMetadata, otto_core::session::SessionError> {
        self.0.append_compaction(checkpoint).await
    }
}

/// One child's view of the shared child registry: the tools whose names are in
/// `allowed`, in the registry's own order. `None` allows every child tool.
pub struct ChildTools {
    registry: Arc<Registry>,
    allowed: Option<BTreeSet<String>>,
    write_policy: WritePolicy,
    write_paths: Vec<String>,
}

impl ChildTools {
    fn permits(&self, name: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }

    fn permits_write_tool(&self, name: &str, arguments: &RawValue) -> Result<(), String> {
        if !matches!(name, "write" | "edit") {
            return Ok(());
        }
        match self.write_policy {
            WritePolicy::ReadOnly | WritePolicy::ProposeOnly => Err(format!(
                "write_policy {} denies workspace mutation tool {name}",
                self.write_policy.as_str()
            )),
            WritePolicy::SingleWriter => Ok(()),
            WritePolicy::OwnedPaths => {
                let path = mutation_path(arguments)?;
                if self
                    .write_paths
                    .iter()
                    .any(|pattern| path_matches(pattern, &path))
                {
                    Ok(())
                } else {
                    Err(format!(
                        "write_policy owned_paths denies {name} for path {path:?}"
                    ))
                }
            }
        }
    }
}

fn mutation_path(arguments: &RawValue) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(arguments.get())
        .map_err(|_| "invalid tool arguments for write policy".to_string())?;
    let path = value
        .get("path")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "missing required argument: path".to_string())?;
    Ok(path.to_string())
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path
            .strip_prefix(&format!("{prefix}/"))
            .is_some_and(|rest| !rest.contains('/'));
    }
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        return path.starts_with(prefix) && path.ends_with(suffix);
    }
    pattern == path
}

#[async_trait::async_trait]
impl ToolExecutor for ChildTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.registry
            .definitions()
            .into_iter()
            .filter(|definition| self.permits(&definition.name))
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> ToolResult {
        if !self.permits(name) {
            return ToolResult::unknown_tool(name);
        }
        if let Err(message) = self.permits_write_tool(name, arguments) {
            return crate::tool::error_result(message);
        }
        self.registry.execute(name, arguments, cancel).await
    }
}

/// The turn settings every child copies from the parent, and the two injected
/// capabilities [`Options`] needs.
pub struct OptionsTemplate {
    pub model: String,
    pub provider_name: String,
    pub thinking: String,
    pub compaction: CompactionSettings,
    pub request_sizer: Option<Arc<dyn RequestSizer + Send + Sync>>,
    pub now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
}

impl Default for OptionsTemplate {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider_name: String::new(),
            thinking: String::new(),
            compaction: CompactionSettings::default(),
            request_sizer: None,
            now: Arc::new(Utc::now),
            new_id: Arc::new(String::new),
        }
    }
}

/// Renders the parent's static system prompt for a tool set.
pub type PromptFor = Arc<dyn Fn(&[ToolDefinition]) -> String + Send + Sync>;

/// Returns the parent's current message list.
pub type ParentSession = Arc<dyn Fn() -> Vec<Message> + Send + Sync>;

/// Everything a [`Runner`] needs from the parent runtime.
pub struct Config {
    pub provider: Arc<dyn Provider + Send + Sync>,
    /// The parent's tool set; the runner drops [`EXCLUDED_CHILD_TOOLS`] by name
    /// to build the child registry. These are the children's own tool
    /// instances, not the parent registry's.
    pub tools: Vec<Box<dyn Tool + Send + Sync>>,
    /// The secret values the parent's redactor holds, and whether that set was
    /// complete. A child builds an equivalent redactor; none of them mutate
    /// the parent's.
    pub redaction_values: Vec<String>,
    pub redaction_complete: bool,
    pub template: OptionsTemplate,
    /// Renders the parent's static system prompt for a tool set. Its output is
    /// treated as already redacted by the caller.
    pub prompt_for: PromptFor,
    pub tasks: Arc<Tasks>,
    /// The named definitions; the default value means none.
    pub catalog: Catalog,
    /// Returns the parent's current message list, which is the post-compaction
    /// view it would send on its next request. `None` disables
    /// `context: inherit`.
    pub parent_session: Option<ParentSession>,
    /// Caps concurrent children; zero means four.
    pub max_parallel: usize,
    /// Caps the report text inside notification and wait text; zero means
    /// 16384.
    pub max_output_bytes: usize,
    /// The parent session's usage collector. Each child binds its task id.
    pub usage: Option<crate::usage::Collector>,
    /// Builds a persisted transcript for each [`Runner::start`] child. `None`
    /// keeps every child in memory.
    pub child_session: Option<ChildSession>,
    /// The automatic skill contract check. `None` disables it: `AgentTool`
    /// then triggers no check, regardless of the definition delegated to.
    pub checker: Option<Arc<crate::skill::check::Checker>>,
}

/// One delegation request.
#[derive(Clone, Debug, Default)]
pub struct StartRequest {
    pub prompt: String,
    pub description: String,
    /// An optional task name, unique for the session including finished tasks;
    /// see [`Tasks::add`] for the validation rules. Empty means no name.
    pub name: String,
    /// The provider model id the child runs on. Empty, or whitespace only,
    /// falls back to the definition's model, then the template's.
    pub model: String,
    /// A catalog definition name, or empty for the default sub-agent.
    pub agent: String,
    /// `"fresh"` or `"inherit"`. Empty falls back to the definition's setting,
    /// then `"fresh"`.
    pub context: String,
}

/// Builds and runs sub-agent tasks for one session.
pub struct Runner {
    config: Config,
    child_registry: Arc<Registry>,
    semaphore: Arc<Semaphore>,
}

impl Runner {
    /// Validates `config`, builds the child tool registry, and resolves each
    /// definition's allowlist. The returned warnings name every definition
    /// tools entry that does not match a child tool.
    pub fn new(mut config: Config) -> Result<(Self, Vec<String>), String> {
        if config.max_parallel == 0 {
            config.max_parallel = DEFAULT_MAX_PARALLEL;
        }
        if config.max_output_bytes == 0 {
            config.max_output_bytes = DEFAULT_MAX_OUTPUT_BYTES;
        }

        let child_tools: Vec<Box<dyn Tool + Send + Sync>> = std::mem::take(&mut config.tools)
            .into_iter()
            .filter(|tool| !EXCLUDED_CHILD_TOOLS.contains(&tool.definition().name.as_str()))
            .collect();
        let child_registry = Arc::new(
            Registry::new(child_tools)
                .map_err(|error| format!("subagent: child registry: {error}"))?,
        );
        let child_names: BTreeSet<String> = child_registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();

        let mut warnings = Vec::new();
        for definition in config.catalog.definitions() {
            let Some(wanted) = &definition.tools else {
                continue;
            };
            for name in wanted {
                if !child_names.contains(name) {
                    warnings.push(format!(
                        "agent {}: unknown tool {name:?} ignored",
                        definition.name
                    ));
                }
            }
        }

        let semaphore = Arc::new(Semaphore::new(config.max_parallel));
        Ok((
            Self {
                config,
                child_registry,
                semaphore,
            },
            warnings,
        ))
    }

    /// The catalog this runner delegates against.
    pub fn catalog(&self) -> &Catalog {
        &self.config.catalog
    }

    /// The session's task registry.
    pub fn tasks(&self) -> &Arc<Tasks> {
        &self.config.tasks
    }

    /// The concurrent-child cap in force.
    pub fn max_parallel(&self) -> usize {
        self.config.max_parallel
    }

    /// The report-text cap in force.
    pub fn max_output_bytes(&self) -> usize {
        self.config.max_output_bytes
    }

    /// The runner's clock, which the status tool uses for elapsed columns.
    pub fn now(&self) -> DateTime<Utc> {
        (self.config.template.now)()
    }

    /// The automatic skill contract checker, when the experimental feature is
    /// enabled.
    pub fn checker(&self) -> Option<&Arc<crate::skill::check::Checker>> {
        self.config.checker.as_ref()
    }

    /// The tool definitions a child with no named definition would see.
    pub fn child_definitions(&self) -> Vec<ToolDefinition> {
        self.child_registry.definitions()
    }

    fn allowed_tools(&self, definition: &Definition) -> Option<BTreeSet<String>> {
        let wanted = definition.tools.as_ref()?;
        let available: BTreeSet<String> = self
            .child_registry
            .definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        Some(
            wanted
                .iter()
                .filter(|name| available.contains(*name))
                .cloned()
                .collect(),
        )
    }

    /// Registers a task and spawns its child, or leaves it queued when
    /// [`Config::max_parallel`] children are already running. An unknown agent
    /// name, an invalid context, or `inherit` with no parent session is
    /// rejected before any task is created.
    pub fn start(self: &Arc<Self>, request: StartRequest) -> Result<Task, StartError> {
        self.start_resolved(request, None, None)
    }

    /// Starts one child with a caller-owned session. Durable workflows use
    /// this path so the child transcript survives the process.
    pub fn start_with_session(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
    ) -> Result<Task, StartError> {
        self.start_resolved(request, Some(transcript), None)
    }

    fn start_resolved(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Option<Transcript>,
        inline_definition: Option<Definition>,
    ) -> Result<Task, StartError> {
        let now = self.now();
        let description = truncate_with_ellipsis(request.description.trim(), MAX_DESCRIPTION_CHARS);

        let agent_name = request.agent.trim();
        let definition: Option<Definition> = if let Some(definition) = inline_definition {
            if agent_name != definition.name {
                return Err(StartError::UnknownAgent(agent_name.to_string()));
            }
            Some(definition)
        } else if agent_name.is_empty() {
            None
        } else {
            Some(
                self.config
                    .catalog
                    .lookup(agent_name)
                    .ok_or_else(|| StartError::UnknownAgent(agent_name.to_string()))?
                    .clone(),
            )
        };

        let model = first_non_empty(&[
            request.model.trim(),
            definition.as_ref().map_or("", |d| d.model.trim()),
            &self.config.template.model,
        ]);
        let context = first_non_empty(&[
            request.context.trim(),
            definition.as_ref().map_or("", |d| d.context.trim()),
            "fresh",
        ]);
        if context != "fresh" && context != "inherit" {
            return Err(StartError::InvalidContext);
        }
        let Some(parent_session) = self.config.parent_session.as_ref() else {
            if context == "inherit" {
                return Err(StartError::InheritUnavailable);
            }
            return self.spawn(
                request,
                definition,
                description,
                model,
                context,
                now,
                Vec::new(),
                transcript,
            );
        };
        let snapshot = if context == "inherit" {
            inherit_snapshot(&parent_session()).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.spawn(
            request,
            definition,
            description,
            model,
            context,
            now,
            snapshot,
            transcript,
        )
    }

    /// Runs one child to a terminal state using `transcript` and returns its
    /// final task record. Cancellation stops the child, not only the wait.
    pub async fn run_with_session(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
        cancel: &CancellationToken,
    ) -> Result<Task, String> {
        self.run_resolved(request, transcript, None, cancel).await
    }

    /// Runs using a definition snapshot captured when a durable workflow was
    /// created, so editing `AGENT.md` cannot change a resumed run.
    pub async fn run_with_definition(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
        definition: Definition,
        cancel: &CancellationToken,
    ) -> Result<Task, String> {
        self.run_resolved(request, transcript, Some(definition), cancel)
            .await
    }

    async fn run_resolved(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
        definition: Option<Definition>,
        cancel: &CancellationToken,
    ) -> Result<Task, String> {
        let task = self
            .start_resolved(request, Some(transcript), definition)
            .map_err(|error| error.to_string())?;
        let id = task.id.clone();
        let result = match self.config.tasks.wait(&id, cancel).await {
            Ok(task) => Ok(task),
            Err(TaskError::Canceled) => {
                self.config
                    .tasks
                    .cancel(&id)
                    .map_err(|error| error.to_string())?;
                self.config
                    .tasks
                    .wait(&id, &CancellationToken::new())
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        };
        if result.is_ok() {
            self.config.tasks.remove_final(&id);
        }
        result
    }

    /// Creates the task record, builds the child agent, and spawns it.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        self: &Arc<Self>,
        request: StartRequest,
        definition: Option<Definition>,
        description: String,
        model: String,
        context: String,
        now: DateTime<Utc>,
        snapshot: Vec<Message>,
        transcript: Option<Transcript>,
    ) -> Result<Task, StartError> {
        let cancel = CancellationToken::new();
        // The task id names the child's file, so the transcript is built
        // after the task is registered; history reads it once it is set.
        let slot: Arc<std::sync::OnceLock<Transcript>> = Arc::default();
        let history_source = Arc::clone(&slot);
        let task = self.config.tasks.add(
            Task {
                name: request.name.trim().to_string(),
                agent: definition
                    .as_ref()
                    .map_or(String::new(), |d| d.name.clone()),
                description,
                prompt: request.prompt.clone(),
                context,
                model: model.clone(),
                created_at: Some(now),
                ..Task::default()
            },
            Some(cancel.clone()),
            Some(Arc::new(move || {
                history_source
                    .get()
                    .map(|transcript| transcript.messages())
                    .unwrap_or_default()
            })),
        )?;
        let transcript = match (transcript, &self.config.child_session) {
            (Some(transcript), _) => transcript,
            (None, None) => Arc::new(MemorySession::new()),
            (None, Some(build)) => match build(&task.id) {
                Ok(Some((transcript, path))) => {
                    self.config.tasks.set_session_path(&task.id, &path);
                    transcript
                }
                Ok(None) => Arc::new(MemorySession::new()),
                Err(error) => {
                    self.finish(&task.id, TaskStatus::Failed, now, &[], Some(&error));
                    return Ok(self.config.tasks.get(&task.id).unwrap_or(task));
                }
            },
        };
        let _ = slot.set(Arc::clone(&transcript));

        let tools = ChildTools {
            registry: Arc::clone(&self.child_registry),
            allowed: definition
                .as_ref()
                .and_then(|definition| self.allowed_tools(definition)),
            write_policy: definition
                .as_ref()
                .map_or(WritePolicy::SingleWriter, |definition| {
                    definition.write_policy
                }),
            write_paths: definition
                .as_ref()
                .map_or_else(Vec::new, |definition| definition.write_paths.clone()),
        };

        let role_body = definition
            .as_ref()
            .map(|d| d.body.as_str())
            .filter(|body| !body.is_empty())
            .unwrap_or(GENERIC_SUBAGENT_INSTRUCTION);
        let redactor = self.redactor();
        let system_prompt = redactor.redact_string(&format!(
            "{}\n\n## Sub-agent role\n{role_body}",
            (self.config.prompt_for)(&tools.definitions())
        ));

        let template = &self.config.template;
        let now_clock = Arc::clone(&template.now);
        let new_id = Arc::clone(&template.new_id);
        let child_inbox = self
            .config
            .tasks
            .child_inbox(&task.id)
            .unwrap_or_else(|| Arc::new(Inbox::new(None)));
        let options = Options {
            model: redactor.redact_string(&model),
            provider_name: template.provider_name.clone(),
            system_prompt,
            thinking: redactor.redact_string(&template.thinking),
            now: Box::new(move || now_clock()),
            new_id: Box::new(move || new_id()),
            request_sizer: template.request_sizer.clone(),
            compaction: template.compaction,
            // A child gets no memory binding, no registry of its own, and a
            // private inbox: it can neither recall, delegate, nor observe the
            // parent's notifications, but the parent can send messages into
            // this private queue with agent_send.
            memory: None,
            tasks: None,
            inbox: child_inbox,
            ..Options::default()
        };

        let child = Agent::with_redactor(
            SharedProvider(Arc::clone(&self.config.provider)),
            tools,
            SharedTranscript(Arc::clone(&transcript)),
            options,
            self.redactor(),
        );

        let runner = Arc::clone(self);
        let task_id = task.id.clone();
        let prompt = request.prompt;
        tokio::spawn(async move {
            runner
                .run_child(cancel, task_id, prompt, child, transcript, snapshot)
                .await;
        });

        Ok(task)
    }

    fn redactor(&self) -> Redactor {
        Redactor::with_completeness(
            &self.config.redaction_values,
            self.config.redaction_complete,
        )
    }

    /// Replays the inherited snapshot, waits for a semaphore slot or for
    /// cancellation while queued, runs the child to completion, and always
    /// finishes the task record.
    async fn run_child(
        &self,
        cancel: CancellationToken,
        task_id: String,
        prompt: String,
        child: Agent<SharedProvider, ChildTools, SharedTranscript>,
        transcript: Transcript,
        snapshot: Vec<Message>,
    ) {
        for message in snapshot {
            if let Err(error) = transcript.append(message).await {
                cancel.cancel();
                let messages = transcript.messages();
                self.finish(
                    &task_id,
                    TaskStatus::Failed,
                    self.now(),
                    &messages,
                    Some(&error.to_string()),
                );
                return;
            }
        }

        let permit = tokio::select! {
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit,
            () = cancel.cancelled() => {
                let messages = transcript.messages();
                self.finish(&task_id, TaskStatus::Canceled, self.now(), &messages, None);
                return;
            }
        };

        self.config.tasks.mark_running(&task_id, self.now());

        let mut progress = ChildProgress::new(Arc::clone(&self.config.tasks), task_id.clone());
        let usage = self
            .config
            .usage
            .as_ref()
            .map(|collector| collector.for_task(&task_id));
        let outcome = {
            let mut handle = |event: Event| {
                if let Some(usage) = &usage {
                    let _ = usage.record(&event);
                }
                progress.handle(event);
            };
            let sink: EventSink<'_> = &mut handle;
            child.run(&prompt, sink, &cancel).await
        };
        drop(permit);
        let _ = child.close();

        let status = match &outcome {
            Ok(()) => TaskStatus::Succeeded,
            Err(_) if cancel.is_cancelled() => TaskStatus::Canceled,
            Err(_) => TaskStatus::Failed,
        };
        let error = outcome.err().map(|error| error.to_string());
        let messages = transcript.messages();
        self.finish(&task_id, status, self.now(), &messages, error.as_deref());
    }

    /// Marks a task final and pushes its completion notification. The
    /// notification is pushed before the registry update that releases
    /// [`Tasks::wait`], so a caller unblocked by a wait always finds it
    /// already in the inbox.
    fn finish(
        &self,
        task_id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        messages: &[Message],
        run_error: Option<&str>,
    ) {
        let mut result = last_assistant_text(messages);
        if result.is_empty() {
            result = "(sub-agent returned no final text)".to_string();
        }
        let error_text = match (status, run_error) {
            (TaskStatus::Failed, Some(error)) => error.to_string(),
            _ => String::new(),
        };

        let mut final_task = self.config.tasks.get(task_id).unwrap_or_default();
        final_task.status = status;
        final_task.finished_at = Some(finished_at);
        final_task.result = result.clone();
        final_task.error = error_text.clone();

        self.config.tasks.notifications().push(Notification {
            task_id: task_id.to_string(),
            kind: Some(NotificationKind::TaskFinished),
            text: completion_text(&final_task, self.config.max_output_bytes),
            usage: final_task.usage_present.then_some(final_task.usage),
        });
        self.config
            .tasks
            .finish(task_id, status, finished_at, &result, &error_text);
    }
}

/// Why [`Runner::start`] refused a request. Every variant is reported before a
/// task record is created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("unknown agent {0:?}")]
    UnknownAgent(String),
    #[error(r#"context must be "fresh" or "inherit""#)]
    InvalidContext,
    #[error("context inherit is not available in this session")]
    InheritUnavailable,
    #[error("{0}")]
    Registry(#[from] TaskError),
}

/// Turns a child's events into task record updates. It never forwards an event
/// to the parent's frontend.
struct ChildProgress {
    tasks: Arc<Tasks>,
    task_id: String,
    text: String,
}

impl ChildProgress {
    fn new(tasks: Arc<Tasks>, task_id: String) -> Self {
        Self {
            tasks,
            task_id,
            text: String::new(),
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::TextDelta { text } => self.text.push_str(&text),
            Event::ProviderUsage { usage, present } => {
                let text = cap_last_bytes(&self.text, 500).to_string();
                self.text.clear();
                self.tasks
                    .record_provider_step(&self.task_id, usage, &text, present);
            }
            Event::ToolCallStarted {
                tool_name,
                arguments,
                ..
            } => {
                let preview = compact_args_preview(&arguments, 60);
                let last_tool = if preview.is_empty() {
                    tool_name
                } else {
                    format!("{tool_name} {preview}")
                };
                self.tasks.record_tool_call(&self.task_id, &last_tool);
            }
            _ => {}
        }
    }
}

/// The notification and wait text for a final task.
pub fn completion_text(task: &Task, max_output_bytes: usize) -> String {
    let name = agent_label(task);
    let duration = completion_duration(task);
    let calls = pluralize_tool_calls(task.tool_calls);
    let model_segment = if task.model.is_empty() {
        String::new()
    } else {
        format!(" · {}", task.model)
    };

    match task.status {
        TaskStatus::Failed => format!(
            "[task-notification] task {} {name} failed{model_segment} · {duration} · {calls}\n{}",
            task.id, task.error
        ),
        // Every status but succeeded and failed renders this way; only a final
        // task ever reaches this function.
        TaskStatus::Canceled | TaskStatus::Queued | TaskStatus::Running => format!(
            "[task-notification] task {} {name} canceled{model_segment} · {duration} · {calls}",
            task.id
        ),
        TaskStatus::Succeeded => {
            let tokens = comma_int(task.usage.input_tokens + task.usage.output_tokens);
            format!(
                "[task-notification] task {} {name} succeeded{model_segment} · {duration} · {calls} · {tokens} tokens\n{}",
                task.id,
                capped_text_result(&task.result, max_output_bytes).content
            )
        }
    }
}

/// The label shown after a task id: the parenthesized definition name, such as
/// `(default)` or `(explorer)`, prefixed by the task's name and a space when
/// it has one, such as `lint-check (explorer)`.
pub fn agent_label(task: &Task) -> String {
    let label = definition_label(task);
    if task.name.is_empty() {
        label
    } else {
        format!("{} {label}", task.name)
    }
}

/// The parenthesized definition name alone. The status table uses it for its
/// definition column, so a task name appears only once per line.
pub fn definition_label(task: &Task) -> String {
    if task.agent.is_empty() {
        "(default)".to_string()
    } else {
        format!("({})", task.agent)
    }
}

/// The finished duration rounded to the second, or `"0s"` for a task canceled
/// while still queued, which never got a start time.
fn completion_duration(task: &Task) -> String {
    let Some(started) = task.started_at else {
        return "0s".to_string();
    };
    let finished = task.finished_at.unwrap_or(started);
    round_to_seconds(finished.signed_duration_since(started))
}

/// The trimmed text of the last assistant message, or `""` when there is none.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map_or(String::new(), |message| message.text().trim().to_string())
}

pub(crate) fn pluralize_tool_calls(count: i64) -> String {
    if count == 1 {
        "1 tool call".to_string()
    } else {
        format!("{count} tool calls")
    }
}

pub(crate) fn pluralize_tools(count: i64) -> String {
    if count == 1 {
        "1 tool".to_string()
    } else {
        format!("{count} tools")
    }
}

/// `value` unchanged when it has at most `max_chars` characters, otherwise its
/// first `max_chars - 1` characters plus `…`, so the result has exactly
/// `max_chars` characters.
fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    format!("{}…", first_runes(value, max_chars - 1))
}

/// The last `max_bytes` bytes of `value`, advanced to the next character
/// boundary so the result never starts mid-character.
pub(crate) fn cap_last_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len() - max_bytes;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

/// Raw JSON with its insignificant whitespace removed. Invalid JSON falls back
/// to collapsing the raw text onto one line.
pub(crate) fn compact_json(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .as_ref()
        .map(serde_json::to_string)
    {
        Some(Ok(text)) => text,
        _ => one_line(raw),
    }
}

/// Tool call arguments as compact JSON capped at `max_chars` characters.
fn compact_args_preview(raw: &str, max_chars: usize) -> String {
    truncate_with_ellipsis(&compact_json(raw), max_chars)
}

fn first_non_empty(candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or(&"")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::testsupport::{
        FakeProvider, RouteStep, StubTool, assistant_text, assistant_tool_call, match_any,
        match_prompt, raw, redaction, redactor, stub, test_config, test_prompt_for, tool_names,
        wait_status,
    };
    use otto_core::agent::inbox::NotificationKind;
    use otto_core::model::{Block, BlockType, FinishReason, Message, Role, Usage};
    use otto_core::provider::Response;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// There is no publish window to race: `MemorySession` carries no header,
    /// so the transcript is created before `Tasks::add` publishes the history
    /// closure.
    const _: () = ();

    fn runner(config: Config) -> (Arc<Runner>, Vec<String>) {
        let (runner, warnings) = Runner::new(config).expect("the test config is valid");
        (Arc::new(runner), warnings)
    }

    /// Waits for `id` to finish, failing the test after five seconds.
    async fn wait_final(tasks: &Tasks, id: &str) -> Task {
        let cancel = CancellationToken::new();
        tokio::select! {
            result = tasks.wait(id, &cancel) => result.expect("the task exists"),
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("task {id} did not finish in time")
            }
        }
    }

    fn definition(name: &str) -> Definition {
        Definition {
            name: name.to_string(),
            ..Definition::default()
        }
    }

    fn catalog(definitions: Vec<Definition>) -> Catalog {
        Catalog::from_definitions(definitions)
    }

    #[tokio::test]
    async fn agent_starts_task_and_pushes_matching_notification() {
        let provider = FakeProvider::new();
        provider.add_route(
            match_prompt("do the thing"),
            vec![assistant_text(
                "all done",
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Usage::default()
                },
            )],
        );

        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

        let tools = crate::subagent::tools::tools(&runner);
        let agent_tool = tools
            .iter()
            .find(|tool| tool.definition().name == "agent")
            .expect("the agent tool is registered");
        let result = agent_tool
            .execute(
                &raw(r#"{"prompt":"do the thing"}"#),
                &CancellationToken::new(),
            )
            .await;
        assert!(
            !result.is_error,
            "agent tool returned error: {}",
            result.content
        );
        assert_eq!(result.content, "task t1 (default) started");

        let final_task = wait_final(&tasks, "t1").await;
        assert_eq!(final_task.status, TaskStatus::Succeeded);
        assert_eq!(final_task.result, "all done");

        let notification = tasks
            .notifications()
            .remove("t1", NotificationKind::TaskFinished)
            .expect("a task_finished notification");
        assert_eq!(
            notification.text,
            completion_text(&final_task, runner.max_output_bytes())
        );
    }

    #[tokio::test]
    async fn child_provider_usage_reaches_the_shared_collector() {
        let provider = FakeProvider::new();
        provider.add_route(
            match_any,
            vec![assistant_text(
                "done",
                Usage {
                    input_tokens: 20,
                    output_tokens: 4,
                    cached_input_tokens: 10,
                },
            )],
        );
        let store = Arc::new(crate::usage::Store::open_in_memory().expect("usage store"));
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.usage = Some(crate::usage::Collector::new(
            Arc::clone(&store),
            crate::usage::Context {
                session_id: "parent".into(),
                ..crate::usage::Context::default()
            },
        ));
        let (runner, _) = runner(config);

        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start");
        wait_final(&tasks, "t1").await;

        let summary = store.summary(Some("parent")).expect("summary");
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.input_tokens, 20);
        assert_eq!(summary.cached_input_tokens, 10);
    }

    #[tokio::test]
    async fn task_finished_notification_omits_absent_usage() {
        let provider = FakeProvider::new();
        provider.add_route(
            match_any,
            vec![RouteStep::Reply(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block::text("done")],
                    ..Message::default()
                },
            })],
        );

        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");

        wait_final(&tasks, "t1").await;
        let notification = tasks
            .notifications()
            .remove("t1", NotificationKind::TaskFinished)
            .expect("a task_finished notification");
        assert_eq!(notification.usage, None);
    }

    #[tokio::test]
    async fn write_policy_denies_mutation_tools_before_execution() {
        let registry = Arc::new(Registry::new(vec![stub("read"), stub("write")]).unwrap());
        let tools = ChildTools {
            registry,
            allowed: None,
            write_policy: WritePolicy::ProposeOnly,
            write_paths: Vec::new(),
        };

        let result = tools
            .execute(
                "write",
                &raw(r#"{"path":"src/lib.rs","content":"x"}"#),
                &CancellationToken::new(),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("propose_only"), "{result:?}");
    }

    #[tokio::test]
    async fn owned_paths_allows_only_matching_mutations() {
        let registry = Arc::new(Registry::new(vec![stub("write")]).unwrap());
        let tools = ChildTools {
            registry,
            allowed: None,
            write_policy: WritePolicy::OwnedPaths,
            write_paths: vec!["crates/otto/**".to_string(), "docs/*.md".to_string()],
        };

        let allowed = tools
            .execute(
                "write",
                &raw(r#"{"path":"crates/otto/src/lib.rs","content":"x"}"#),
                &CancellationToken::new(),
            )
            .await;
        assert!(!allowed.is_error, "{allowed:?}");

        let denied = tools
            .execute(
                "write",
                &raw(r#"{"path":"crates/otto-core/src/lib.rs","content":"x"}"#),
                &CancellationToken::new(),
            )
            .await;
        assert!(denied.is_error, "{denied:?}");
        assert!(denied.content.contains("owned_paths"), "{denied:?}");
    }

    #[tokio::test]
    async fn child_registry_excludes_control_and_memory_tools() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());

        let mut parent: Vec<Box<dyn Tool + Send + Sync>> = vec![stub("read"), stub("write")];
        for name in EXCLUDED_CHILD_TOOLS {
            parent.push(stub(name));
        }
        parent.push(stub("grep"));

        let (runner, _) = runner(test_config(&provider, &tasks, parent));
        assert_eq!(
            tool_names(&runner.child_definitions()).join(","),
            "read,write,grep"
        );
    }

    #[tokio::test]
    async fn child_system_prompt_shape() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let requests = provider.requests();
        let prompt = &requests
            .first()
            .expect("one provider request")
            .system_prompt;
        assert!(
            prompt.starts_with(&test_prompt_for(&runner.child_definitions())),
            "system prompt does not start with the parent prompt:\n{prompt}"
        );
        assert!(prompt.contains("## Sub-agent role"), "{prompt}");
        assert!(prompt.contains(GENERIC_SUBAGENT_INSTRUCTION), "{prompt}");
    }

    /// [`Redactor`] is immutable and each child builds its own from the
    /// configured values, so what is checked here is the observable half: a
    /// secret reaching the runner as the parent model or as a per-call model is
    /// redacted before it leaves in a provider request.
    #[tokio::test]
    async fn subagent_run_redacts_the_model() {
        let secret = "sk-supersecret123";
        for per_call in [false, true] {
            let provider = FakeProvider::new();
            provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
            let tasks = Arc::new(Tasks::new());
            let mut config = test_config(&provider, &tasks, Vec::new());
            config.redaction_values = redaction(&[secret]);
            config.template.model = if per_call {
                "gpt-parent".into()
            } else {
                secret.into()
            };
            let (runner, _) = runner(config);

            runner
                .start(StartRequest {
                    prompt: "go".into(),
                    model: if per_call {
                        secret.into()
                    } else {
                        String::new()
                    },
                    ..StartRequest::default()
                })
                .expect("start succeeds");

            let final_task = wait_final(&tasks, "t1").await;
            assert_eq!(final_task.status, TaskStatus::Succeeded);

            let want = redactor(&[secret]).redact_string(secret);
            assert_ne!(
                want, secret,
                "the redactor must rewrite its configured secret"
            );
            let requests = provider.requests();
            assert_eq!(requests.first().expect("one request").model, want);
        }
    }

    #[tokio::test]
    async fn max_parallel_limits_concurrency() {
        let provider = FakeProvider::new();
        let release = CancellationToken::new();
        let counts = Arc::new(Mutex::new((0usize, 0usize)));

        let hook_release = release.clone();
        let hook_counts = Arc::clone(&counts);
        provider.set_hook(Arc::new(move |cancel, _request| {
            let release = hook_release.clone();
            let counts = Arc::clone(&hook_counts);
            Box::pin(async move {
                {
                    let mut guard = counts.lock().expect("the counter lock is intact");
                    guard.0 += 1;
                    guard.1 = guard.1.max(guard.0);
                }
                tokio::select! {
                    () = release.cancelled() => {}
                    () = cancel.cancelled() => {}
                }
                counts.lock().expect("the counter lock is intact").0 -= 1;
            })
        }));
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.max_parallel = 2;
        let (runner, _) = runner(config);

        // t1 and t2 must hold both slots before t3 starts: starting all three
        // at once races three tasks against a two-slot semaphore with no
        // ordering tied to start() call order.
        for index in 0..2 {
            runner
                .start(StartRequest {
                    prompt: format!("task {index}"),
                    ..StartRequest::default()
                })
                .expect("start succeeds");
        }
        wait_status(&tasks, "t1", TaskStatus::Running).await;
        wait_status(&tasks, "t2", TaskStatus::Running).await;

        runner
            .start(StartRequest {
                prompt: "task 2".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        assert_eq!(
            tasks.get("t3").expect("t3 exists").status,
            TaskStatus::Queued,
            "t3 must stay queued while t1 and t2 hold the only two slots"
        );

        release.cancel();
        for id in ["t1", "t2", "t3"] {
            assert_eq!(wait_final(&tasks, id).await.status, TaskStatus::Succeeded);
        }
        assert!(
            counts.lock().expect("the counter lock is intact").1 <= 2,
            "observed more than two concurrent provider calls"
        );
    }

    #[tokio::test]
    async fn cancel_running_task() {
        let provider = FakeProvider::new();
        provider.set_hook(Arc::new(|cancel, _request| {
            Box::pin(async move { cancel.cancelled().await })
        }));

        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");

        wait_status(&tasks, "t1", TaskStatus::Running).await;
        tasks.cancel("t1").expect("cancel succeeds");
        assert_eq!(wait_final(&tasks, "t1").await.status, TaskStatus::Canceled);
    }

    #[tokio::test]
    async fn cancel_queued_task() {
        let provider = FakeProvider::new();
        let block = CancellationToken::new();
        let hook_block = block.clone();
        provider.set_hook(Arc::new(move |cancel, _request| {
            let block = hook_block.clone();
            Box::pin(async move {
                tokio::select! {
                    () = block.cancelled() => {}
                    () = cancel.cancelled() => {}
                }
            })
        }));
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.max_parallel = 1;
        let (runner, _) = runner(config);

        runner
            .start(StartRequest {
                prompt: "first".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        wait_status(&tasks, "t1", TaskStatus::Running).await;

        runner
            .start(StartRequest {
                prompt: "second".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        assert_eq!(
            tasks.get("t2").expect("t2 exists").status,
            TaskStatus::Queued
        );

        tasks.cancel("t2").expect("cancel succeeds");
        let final_task = wait_final(&tasks, "t2").await;
        assert_eq!(final_task.status, TaskStatus::Canceled);
        assert_eq!(
            final_task.started_at, None,
            "a task canceled while queued never started"
        );

        block.cancel();
        wait_final(&tasks, "t1").await;
    }

    #[tokio::test]
    async fn provider_error_fails_task() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![RouteStep::Fail("provider exploded".into())]);

        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");

        let final_task = wait_final(&tasks, "t1").await;
        assert_eq!(final_task.status, TaskStatus::Failed);
        assert!(
            final_task.error.contains("provider exploded"),
            "error = {:?}",
            final_task.error
        );
    }

    #[tokio::test]
    async fn task_record_fields_update_across_provider_steps() {
        let provider = FakeProvider::new();
        provider.add_route(
            match_any,
            vec![
                assistant_tool_call(
                    "call-1",
                    "echo",
                    r#"{"msg":"hi"}"#,
                    Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Usage::default()
                    },
                ),
                assistant_text(
                    "final report",
                    Usage {
                        input_tokens: 20,
                        output_tokens: 8,
                        ..Usage::default()
                    },
                ),
            ],
        );

        let echo = StubTool::new(
            "echo",
            ToolResult {
                content: "echoed".into(),
                ..ToolResult::default()
            },
        );
        let calls = echo.counter();

        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, vec![echo.boxed()]));
        runner
            .start(StartRequest {
                prompt: "go".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");

        let final_task = wait_final(&tasks, "t1").await;
        assert_eq!(final_task.status, TaskStatus::Succeeded);
        assert_eq!(final_task.steps, 2);
        assert_eq!(final_task.tool_calls, 1);
        assert!(
            final_task.last_tool.contains("echo"),
            "{:?}",
            final_task.last_tool
        );
        assert_eq!(final_task.last_text, "final report");
        assert_eq!(final_task.usage.input_tokens, 30);
        assert_eq!(final_task.usage.output_tokens, 13);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn start_unknown_agent_rejected_before_task_created() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

        let error = runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "nope".into(),
                ..StartRequest::default()
            })
            .expect_err("an unknown agent is rejected");
        assert_eq!(error.to_string(), r#"unknown agent "nope""#);
        assert!(tasks.list().is_empty());
    }

    #[tokio::test]
    async fn start_named_agent_sets_task_agent() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.catalog = catalog(vec![definition("reviewer")]);
        let (runner, _) = runner(config);

        let task = runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        assert_eq!(task.agent, "reviewer");
        assert_eq!(wait_final(&tasks, &task.id).await.agent, "reviewer");
    }

    #[tokio::test]
    async fn start_model_precedence() {
        for (call_model, definition_model, session_model, want) in [
            ("call-model", "def-model", "sess-model", "call-model"),
            ("", "def-model", "sess-model", "def-model"),
            ("", "", "sess-model", "sess-model"),
        ] {
            let provider = FakeProvider::new();
            provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
            let tasks = Arc::new(Tasks::new());
            let mut config = test_config(&provider, &tasks, Vec::new());
            config.template.model = session_model.into();
            config.catalog = catalog(vec![Definition {
                name: "reviewer".into(),
                model: definition_model.into(),
                ..Definition::default()
            }]);
            let (runner, _) = runner(config);

            runner
                .start(StartRequest {
                    prompt: "go".into(),
                    agent: "reviewer".into(),
                    model: call_model.into(),
                    ..StartRequest::default()
                })
                .expect("start succeeds");
            wait_final(&tasks, "t1").await;

            assert_eq!(
                provider.requests().first().expect("one request").model,
                want
            );
        }
    }

    /// A definition's tool allowlist narrows the child registry to exactly
    /// those tools, in the parent's order. The assertion reads the child's
    /// system prompt, because the test prompt names the child's tools.
    #[tokio::test]
    async fn new_runner_tools_allowlist_parent_order() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(
            &provider,
            &tasks,
            vec![stub("grep"), stub("read"), stub("write")],
        );
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            tools: Some(vec!["read".into(), "grep".into()]),
            ..Definition::default()
        }]);
        let (runner, warnings) = runner(config);
        assert!(warnings.is_empty(), "{warnings:?}");

        runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let requests = provider.requests();
        assert!(
            requests
                .first()
                .expect("one request")
                .system_prompt
                .starts_with("PARENT PROMPT tools=grep,read\n"),
            "{}",
            requests[0].system_prompt
        );
    }

    #[tokio::test]
    async fn new_runner_unknown_tool_warning() {
        for bad in ["bogus", "agent", "remember"] {
            let provider = FakeProvider::new();
            provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
            let tasks = Arc::new(Tasks::new());
            let mut config = test_config(&provider, &tasks, vec![stub("read")]);
            config.catalog = catalog(vec![Definition {
                name: "reviewer".into(),
                tools: Some(vec!["read".into(), bad.into()]),
                ..Definition::default()
            }]);
            let (runner, warnings) = runner(config);
            assert!(
                warnings.contains(&format!("agent reviewer: unknown tool {bad:?} ignored")),
                "{warnings:?}"
            );

            runner
                .start(StartRequest {
                    prompt: "go".into(),
                    agent: "reviewer".into(),
                    ..StartRequest::default()
                })
                .expect("start succeeds");
            wait_final(&tasks, "t1").await;
            assert!(
                provider.requests()[0]
                    .system_prompt
                    .starts_with("PARENT PROMPT tools=read\n"),
                "{}",
                provider.requests()[0].system_prompt
            );
        }
    }

    #[tokio::test]
    async fn start_system_prompt_uses_definition_body() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            body: "You are a reviewer.".into(),
            ..Definition::default()
        }]);
        let (runner, _) = runner(config);
        runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        let prompt = provider.requests()[0].system_prompt.clone();
        assert!(
            prompt.contains("## Sub-agent role\nYou are a reviewer."),
            "{prompt}"
        );
        assert!(!prompt.contains(GENERIC_SUBAGENT_INSTRUCTION), "{prompt}");
    }

    #[tokio::test]
    async fn start_system_prompt_empty_body_uses_generic_instruction() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.catalog = catalog(vec![definition("reviewer")]);
        let (runner, _) = runner(config);
        runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        assert!(
            provider.requests()[0]
                .system_prompt
                .contains(GENERIC_SUBAGENT_INSTRUCTION)
        );
    }

    fn user_message(id: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            role: Role::User,
            blocks: vec![Block::text(text)],
            ..Message::default()
        }
    }

    fn assistant_message(id: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            role: Role::Assistant,
            blocks: vec![Block::text(text)],
            ..Message::default()
        }
    }

    fn tool_call_message(id: &str, tool_name: &str) -> Message {
        Message {
            id: id.into(),
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_name: tool_name.into(),
                tool_call_id: "call-1".into(),
                ..Block::default()
            }],
            ..Message::default()
        }
    }

    #[tokio::test]
    async fn start_context_inherit_snapshot_in_first_request() {
        let parent = vec![
            user_message("u1", "first"),
            assistant_message("a1", "reply one"),
            user_message("u2", "second"),
            tool_call_message("a2", "agent"),
            // A sibling call's result, appended after the pending agent call.
            Message {
                id: "tr1".into(),
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::ToolResult,
                    tool_call_id: "call-0".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
        ];
        let want_snapshot = parent[..3].to_vec();

        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        let source = parent.clone();
        config.parent_session = Some(Arc::new(move || source.clone()));
        let (runner, _) = runner(config);

        let task = runner
            .start(StartRequest {
                prompt: "delegated task".into(),
                context: "inherit".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        assert_eq!(task.context, "inherit");
        wait_final(&tasks, &task.id).await;

        let messages = provider.requests()[0].messages.clone();
        assert_eq!(messages.len(), want_snapshot.len() + 1, "{messages:#?}");
        assert_eq!(&messages[..want_snapshot.len()], want_snapshot.as_slice());
        let last = &messages[want_snapshot.len()];
        assert_eq!(last.role, Role::User);
        assert_eq!(last.text(), "delegated task");
    }

    /// A child store for task `task_id` beside `parent`, or one whose writes
    /// fail when `fail` is set.
    fn child_store(parent: &std::path::Path, task_id: &str, fail: bool) -> (Transcript, String) {
        let name = format!("{task_id}-child");
        let store = crate::session::Store::create_child_lazy(
            parent,
            &name,
            otto_core::session::Header {
                id: "child".into(),
                workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("child store");
        store.lock().expect("lock").fail_writes = fail;
        let path = crate::session::Store::child_path(parent, &name);
        (Arc::new(store), path.to_string_lossy().into_owned())
    }

    #[tokio::test]
    async fn a_child_transcript_with_its_inherited_context_is_written_to_its_own_file() {
        let dir = tempfile::tempdir().expect("dir");
        let parent_path = dir.path().join("parent.jsonl");
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        // A stored parent message always carries a timestamp.
        let inherited: Vec<Message> = [
            user_message("u1", "first"),
            tool_call_message("a1", "agent"),
        ]
        .into_iter()
        .map(|message| Message {
            created_at: Utc::now(),
            ..message
        })
        .collect();
        config.parent_session = Some(Arc::new(move || inherited.clone()));
        let parent = parent_path.clone();
        config.child_session = Some(Arc::new(move |task_id: &str| {
            Ok(Some(child_store(&parent, task_id, false)))
        }));
        let (runner, _) = runner(config);

        let task = runner
            .start(StartRequest {
                prompt: "delegated task".into(),
                context: "inherit".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        let done = wait_final(&tasks, &task.id).await;

        let want = dir
            .path()
            .join("parent")
            .join(format!("{}-child.jsonl", task.id));
        assert_eq!(done.status, TaskStatus::Succeeded, "{}", done.error);
        assert_eq!(done.session_path, want.to_string_lossy());
        let (store, _) = crate::session::Store::open(&want).expect("open child");
        let texts: Vec<String> = store.messages().iter().map(Message::text).collect();
        assert_eq!(texts, ["first", "delegated task", "done"]);
        store.close().expect("close");
    }

    #[tokio::test]
    async fn a_child_transcript_write_failure_fails_only_that_task() {
        let dir = tempfile::tempdir().expect("dir");
        let parent_path = dir.path().join("parent.jsonl");
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.child_session = Some(Arc::new(move |task_id: &str| {
            Ok(Some(child_store(&parent_path, task_id, true)))
        }));
        let (runner, _) = runner(config);

        let task = runner
            .start(StartRequest {
                prompt: "delegated task".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        let done = wait_final(&tasks, &task.id).await;

        assert_eq!(done.status, TaskStatus::Failed);
        assert!(!done.error.is_empty());
    }

    #[tokio::test]
    async fn start_context_fresh_only_prompt() {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        let consulted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&consulted);
        config.parent_session = Some(Arc::new(move || {
            flag.store(true, Ordering::SeqCst);
            Vec::new()
        }));
        let (runner, _) = runner(config);

        let task = runner
            .start(StartRequest {
                prompt: "go".into(),
                context: "fresh".into(),
                ..StartRequest::default()
            })
            .expect("start succeeds");
        assert_eq!(task.context, "fresh");
        wait_final(&tasks, &task.id).await;

        assert!(
            !consulted.load(Ordering::SeqCst),
            "the parent session must not be consulted for context fresh"
        );
        let messages = provider.requests()[0].messages.clone();
        assert_eq!(messages.len(), 1, "{messages:#?}");
        assert_eq!(messages[0].text(), "go");
    }

    #[tokio::test]
    async fn start_context_definition_inherit_honoured_and_call_override() {
        let parent = vec![
            user_message("u1", "hello"),
            tool_call_message("a1", "agent"),
        ];

        for (request_context, want_context, want_messages) in
            [("", "inherit", 2usize), ("fresh", "fresh", 1)]
        {
            let provider = FakeProvider::new();
            provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
            let tasks = Arc::new(Tasks::new());
            let mut config = test_config(&provider, &tasks, Vec::new());
            let source = parent.clone();
            config.parent_session = Some(Arc::new(move || source.clone()));
            config.catalog = catalog(vec![Definition {
                name: "reviewer".into(),
                context: "inherit".into(),
                ..Definition::default()
            }]);
            let (runner, _) = runner(config);

            let task = runner
                .start(StartRequest {
                    prompt: "go".into(),
                    agent: "reviewer".into(),
                    context: request_context.into(),
                    ..StartRequest::default()
                })
                .expect("start succeeds");
            assert_eq!(task.context, want_context);
            wait_final(&tasks, &task.id).await;
            assert_eq!(provider.requests()[0].messages.len(), want_messages);
        }
    }

    #[tokio::test]
    async fn start_context_inherit_without_parent_session_errors() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

        let error = runner
            .start(StartRequest {
                prompt: "go".into(),
                context: "inherit".into(),
                ..StartRequest::default()
            })
            .expect_err("inherit without a parent session is rejected");
        assert_eq!(
            error.to_string(),
            "context inherit is not available in this session"
        );
        assert!(tasks.list().is_empty());
    }

    #[tokio::test]
    async fn start_invalid_context_errors() {
        let provider = FakeProvider::new();
        let tasks = Arc::new(Tasks::new());
        let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

        let error = runner
            .start(StartRequest {
                prompt: "go".into(),
                context: "bogus".into(),
                ..StartRequest::default()
            })
            .expect_err("an invalid context is rejected");
        assert_eq!(error.to_string(), r#"context must be "fresh" or "inherit""#);
        assert!(tasks.list().is_empty());
    }

    fn base() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("the base timestamp parses")
    }

    fn finished(seconds: i64) -> Option<DateTime<Utc>> {
        Some(base() + chrono::Duration::seconds(seconds))
    }

    #[test]
    fn completion_text_renders_every_status() {
        let cases: Vec<(&str, Task, usize, &str)> = vec![
            (
                "succeeded",
                Task {
                    id: "t1".into(),
                    status: TaskStatus::Succeeded,
                    created_at: Some(base()),
                    started_at: Some(base()),
                    finished_at: finished(42),
                    tool_calls: 7,
                    usage: Usage {
                        input_tokens: 10000,
                        output_tokens: 2310,
                        ..Usage::default()
                    },
                    result: "the full report".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t1 (default) succeeded · 42s · 7 tool calls · 12,310 tokens\nthe full report",
            ),
            (
                "failed",
                Task {
                    id: "t1".into(),
                    status: TaskStatus::Failed,
                    started_at: Some(base()),
                    finished_at: finished(12),
                    tool_calls: 3,
                    error: "provider exploded".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t1 (default) failed · 12s · 3 tool calls\nprovider exploded",
            ),
            (
                "canceled",
                Task {
                    id: "t1".into(),
                    status: TaskStatus::Canceled,
                    started_at: Some(base()),
                    finished_at: finished(12),
                    tool_calls: 3,
                    ..Task::default()
                },
                16384,
                "[task-notification] task t1 (default) canceled · 12s · 3 tool calls",
            ),
            (
                "canceled while queued",
                Task {
                    id: "t3".into(),
                    status: TaskStatus::Canceled,
                    started_at: None,
                    finished_at: Some(base()),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t3 (default) canceled · 0s · 0 tool calls",
            ),
            (
                "singular tool call",
                Task {
                    id: "t4".into(),
                    status: TaskStatus::Succeeded,
                    started_at: Some(base()),
                    finished_at: finished(1),
                    tool_calls: 1,
                    usage: Usage {
                        input_tokens: 1,
                        ..Usage::default()
                    },
                    result: "ok".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t4 (default) succeeded · 1s · 1 tool call · 1 tokens\nok",
            ),
            (
                "duration over a minute",
                Task {
                    id: "t5".into(),
                    status: TaskStatus::Succeeded,
                    started_at: Some(base()),
                    finished_at: finished(65),
                    tool_calls: 2,
                    result: "done".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t5 (default) succeeded · 1m5s · 2 tool calls · 0 tokens\ndone",
            ),
            (
                "succeeded with model",
                Task {
                    id: "t1".into(),
                    status: TaskStatus::Succeeded,
                    started_at: Some(base()),
                    finished_at: finished(42),
                    tool_calls: 7,
                    model: "gpt-4o-mini".into(),
                    usage: Usage {
                        input_tokens: 10000,
                        output_tokens: 2310,
                        ..Usage::default()
                    },
                    result: "the full report".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t1 (default) succeeded · gpt-4o-mini · 42s · 7 tool calls · 12,310 tokens\nthe full report",
            ),
            (
                "failed with model",
                Task {
                    id: "t1".into(),
                    status: TaskStatus::Failed,
                    started_at: Some(base()),
                    finished_at: finished(12),
                    tool_calls: 3,
                    model: "gpt-4o-mini".into(),
                    error: "provider exploded".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t1 (default) failed · gpt-4o-mini · 12s · 3 tool calls\nprovider exploded",
            ),
            (
                "canceled with model",
                Task {
                    id: "t1".into(),
                    status: TaskStatus::Canceled,
                    started_at: Some(base()),
                    finished_at: finished(12),
                    tool_calls: 3,
                    model: "gpt-4o-mini".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t1 (default) canceled · gpt-4o-mini · 12s · 3 tool calls",
            ),
            (
                "succeeded with a name",
                Task {
                    id: "t1".into(),
                    name: "lint-check".into(),
                    status: TaskStatus::Succeeded,
                    started_at: Some(base()),
                    finished_at: finished(42),
                    tool_calls: 7,
                    usage: Usage {
                        input_tokens: 10000,
                        output_tokens: 2310,
                        ..Usage::default()
                    },
                    result: "the full report".into(),
                    ..Task::default()
                },
                16384,
                "[task-notification] task t1 lint-check (default) succeeded · 42s · 7 tool calls · 12,310 tokens\nthe full report",
            ),
        ];

        for (name, task, max_output_bytes, want) in cases {
            assert_eq!(
                completion_text(&task, max_output_bytes),
                want,
                "case {name}"
            );
        }
    }

    #[test]
    fn completion_text_caps_the_result() {
        let long = "a".repeat(5000);
        let task = Task {
            id: "t6".into(),
            status: TaskStatus::Succeeded,
            started_at: Some(base()),
            finished_at: finished(3),
            result: long.clone(),
            ..Task::default()
        };
        let got = completion_text(&task, 100);
        let want = format!(
            "[task-notification] task t6 (default) succeeded · 3s · 0 tool calls · 0 tokens\n{}",
            capped_text_result(&long, 100).content
        );
        assert_eq!(got, want);
        assert!(got.contains("truncated"), "{got}");
    }
}
