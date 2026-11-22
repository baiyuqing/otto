//! Session binding of hooks: per-call payload with session identity and live history path.
use super::*;
use otto_core::agent::hooks::Edit;

/// Session identity and live values sent in the `session` payload object of every hook call.
pub struct SessionBinding {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub task_id: String,
    pub mode: String,
    /// True for `otto --prompt` runs only.
    pub headless: bool,
    /// Read at each hook call: empty while a lazily created session has no file.
    pub history_path: Arc<dyn Fn() -> String + Send + Sync>,
}

impl Runtime {
    pub fn bind(
        self: &Arc<Self>,
        binding: SessionBinding,
        guard: Option<Arc<SessionGuard>>,
    ) -> Arc<dyn Hooks> {
        Arc::new(Bound {
            runtime: self.clone(),
            guard,
            binding,
        })
    }
}

/// Joins two optional notices with a newline; the earlier one comes first.
fn join_notices(earlier: Option<String>, later: Option<String>) -> Option<String> {
    match (earlier, later) {
        (Some(earlier), Some(later)) => Some(format!("{earlier}\n{later}")),
        (earlier, later) => earlier.or(later),
    }
}

/// Rejects response fields that `event` cannot carry.
pub(super) fn check_response_fields(event: &str, response: &Response) -> Result<(), String> {
    if response.arguments.is_some() && event != "before_tool"
        || response.content.is_some() && event != "after_tool"
        || (response.system_prompt.is_some() || response.edits.is_some())
            && event != "before_provider_call"
        || response.context.is_some()
            && ![
                "session_start",
                "before_prompt",
                "before_provider_call",
                "before_compaction",
            ]
            .contains(&event)
        || (response.decision.is_some() || response.reason.is_some())
            && ![
                "session_start",
                "before_prompt",
                "before_provider_call",
                "before_tool",
                "before_compaction",
            ]
            .contains(&event)
    {
        return Err("hook response field is not supported for this event".into());
    }
    Ok(())
}

/// Adds the edits of one hook response to the edits of the earlier hooks: an
/// edit replaces an earlier hook's edit of the same block. Duplicates inside
/// one response are kept so the agent rejects them.
fn merge_edits(combined: &mut Option<Vec<Edit>>, edits: Vec<Edit>) {
    let merged = combined.get_or_insert_default();
    merged.retain(|earlier| {
        !edits
            .iter()
            .any(|edit| edit.message == earlier.message && edit.block == earlier.block)
    });
    merged.extend(edits);
}

/// Shows later `before_provider_call` hooks the request as earlier hooks changed it.
/// Only blocks an edit may target change: a `tool_result` block, or a `text` block of a
/// `user` message without a `context_type`. Other edits leave `data` unchanged and are
/// left for the agent to reject.
fn apply_to_request_data(data: &mut Value, system_prompt: Option<&str>, edits: &[Edit]) {
    if let Some(system_prompt) = system_prompt
        && let Some(object) = data.as_object_mut()
    {
        object.insert("system_prompt".into(), system_prompt.into());
    }
    for edit in edits {
        let Some(message) = data.pointer_mut(&format!("/messages/{}", edit.message)) else {
            continue;
        };
        let user_input = message.get("role").and_then(Value::as_str) == Some("user")
            && message.get("context_type").is_none();
        let Some(block) = message
            .pointer_mut(&format!("/blocks/{}", edit.block))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let editable = match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => true,
            Some("text") => user_input,
            _ => false,
        };
        if editable {
            block.insert("text".into(), edit.text.clone().into());
        }
    }
}

/// The tool name an Otto hook matcher is tested against: the remote tool name for
/// `mcp_call_tool`, the Otto tool name otherwise.
fn otto_matcher_name(data: &Value) -> &str {
    if data.get("tool").and_then(Value::as_str) == Some("mcp_call_tool") {
        data.pointer("/arguments/tool")
            .and_then(Value::as_str)
            .unwrap_or("mcp_call_tool")
    } else {
        data.get("tool").and_then(Value::as_str).unwrap_or("")
    }
}

pub(super) struct Bound {
    pub(super) runtime: Arc<Runtime>,
    pub(super) guard: Option<Arc<SessionGuard>>,
    pub(super) binding: SessionBinding,
}
impl Bound {
    /// Runs one Otto hook with the protocol v1 payload and parses its JSON response.
    async fn run_otto(
        &self,
        event: &str,
        data: &Value,
        hook: &Hook,
        package: Option<&Package>,
        control: &dyn OperationControl,
    ) -> Result<Response, String> {
        let output = self
            .runtime
            .command(
                super::Handler {
                    command: &hook.command,
                    lua: hook.lua.as_deref(),
                    timeout: &hook.timeout,
                },
                package,
                json!({
                    "version":1,
                    "event":event,
                    "workspace":self.runtime.execution.workspace,
                    "headless":self.binding.headless,
                    "session":{
                        "session_id":self.binding.session_id,
                        "parent_session_id":self.binding.parent_session_id,
                        "task_id":self.binding.task_id,
                        "mode":self.binding.mode,
                        "history_path":(self.binding.history_path)()
                    },
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
        check_response_fields(event, &response)?;
        Ok(response)
    }
}
#[async_trait::async_trait]
impl Hooks for Bound {
    fn handles(&self, event: &str) -> bool {
        self.runtime.hooks.iter().any(|entry| match entry {
            HookEntry::Otto {
                event: hook_event, ..
            } => hook_event == event,
            HookEntry::Claude { otto_event, .. } => *otto_event == event,
        })
    }

    async fn run(
        &self,
        event: &str,
        mut data: Value,
        control: &dyn OperationControl,
    ) -> Result<Response, String> {
        let mut combined = Response::default();
        let mut diagnostics = Vec::new();
        for entry in &self.runtime.hooks {
            let result = match entry {
                HookEntry::Otto {
                    event: hook_event,
                    hook,
                    package,
                } => {
                    if hook_event != event {
                        continue;
                    }
                    let name = otto_matcher_name(&data);
                    if !hook.matcher.is_empty() && !hook.matcher.iter().any(|p| matches(p, name)) {
                        continue;
                    }
                    self.run_otto(event, &data, hook, package.as_ref(), control)
                        .await
                }
                HookEntry::Claude {
                    otto_event,
                    hook,
                    matcher,
                    package,
                } => {
                    if *otto_event != event {
                        continue;
                    }
                    match self
                        .run_claude(event, &data, hook, matcher, package, control)
                        .await
                    {
                        Some(result) => result,
                        None => continue,
                    }
                }
            };
            match result {
                Err(message) if event.starts_with("before_") || event == "session_start" => {
                    return Err(message);
                }
                Err(message) => diagnostics.push(message),
                Ok(mut response) => {
                    let notice = response.notice.take();
                    if response.decision == Some(Decision::Deny) {
                        response.notice = join_notices(combined.notice.take(), notice);
                        return Ok(response);
                    }
                    combined.notice = join_notices(combined.notice.take(), notice);
                    if let Some(arguments) = response.arguments {
                        data["arguments"] = arguments.clone();
                        combined.arguments = Some(arguments);
                    }
                    if let Some(content) = response.content {
                        data["content"] = content.clone().into();
                        combined.content = Some(content);
                    }
                    let edits = response.edits.unwrap_or_default();
                    apply_to_request_data(&mut data, response.system_prompt.as_deref(), &edits);
                    if !edits.is_empty() {
                        merge_edits(&mut combined.edits, edits);
                    }
                    if let Some(system_prompt) = response.system_prompt {
                        combined.system_prompt = Some(system_prompt);
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
