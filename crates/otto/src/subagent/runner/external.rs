//! External ACP agents run within the session's existing task lifecycle.

use super::*;
use otto_core::model::Block;
use serde_json::Value;
use std::path::Path;

pub(crate) fn adapter(name: &str) -> Option<&'static str> {
    match name {
        "acp:codex" => Some("codex-acp"),
        "acp:claude" => Some("claude-agent-acp"),
        "acp:pi" => Some("pi-acp"),
        _ => None,
    }
}

impl Runner {
    pub(super) async fn run_external(
        self: Arc<Self>,
        task_id: String,
        prompt: String,
        executable: &'static str,
        cancel: CancellationToken,
        transcript: Transcript,
        deadline: Deadline,
    ) {
        let permit = tokio::select! {
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit.ok(),
            () = cancel.cancelled() => None,
            () = deadline.expired() => None,
        };
        let Some(_permit) = permit else {
            let status = if cancel.is_cancelled() {
                TaskStatus::Canceled
            } else {
                TaskStatus::Interrupted
            };
            self.finish(
                &task_id,
                status,
                self.now(),
                &transcript.messages(),
                None,
                Some(&transcript),
            )
            .await;
            return;
        };
        self.config.tasks.mark_running(&task_id, self.now());
        let (environment, environment_secrets) = adapter_environment();
        let mut redaction_values = self.config.redaction_values.clone();
        redaction_values.extend(environment_secrets);
        let redactor =
            Redactor::with_completeness(&redaction_values, self.config.redaction_complete);
        let user_message = Message {
            role: Role::User,
            blocks: vec![Block::text(&redactor.redact_string(&prompt))],
            ..Message::default()
        };
        if let Err(error) = transcript.append(user_message).await {
            self.finish(
                &task_id,
                TaskStatus::Failed,
                self.now(),
                &transcript.messages(),
                Some(&error.to_string()),
                Some(&transcript),
            )
            .await;
            return;
        }
        let mut output = String::new();
        let mut output_truncated = false;
        let process_cancel = cancel.child_token();
        let approval = self.config.approval.as_ref().map(|(service, scope)| {
            let mut scope = scope.clone();
            scope.task_id = task_id.clone();
            Arc::new(super::super::acp_approval::Handler {
                service: Arc::clone(service),
                scope,
                redactor: redactor.clone(),
                cancel: process_cancel.clone(),
            }) as Arc<dyn super::super::acp_transport::PermissionHandler>
        });
        let (result, timed_out) = {
            let process = super::super::acp_process::run(
                Path::new(executable),
                &self.config.workspace_path,
                environment,
                &prompt,
                &process_cancel,
                approval,
                |event: Value| {
                    let update = &event["update"];
                    if update["sessionUpdate"] == "agent_message_chunk"
                        && let Some(text) = update["content"]["text"].as_str()
                    {
                        let remaining = (1024 * 1024_usize).saturating_sub(output.len());
                        let end = text.floor_char_boundary(remaining.min(text.len()));
                        output.push_str(&text[..end]);
                        output_truncated |= end < text.len();
                    }
                    if update["sessionUpdate"] == "tool_call"
                        && let Some(title) = update["title"].as_str()
                    {
                        self.config.tasks.record_tool_call(
                            &task_id,
                            &first_runes(&redactor.redact_string(title), 200),
                        );
                    }
                    if update["sessionUpdate"] == "tool_call_update"
                        && matches!(update["status"].as_str(), Some("completed" | "failed"))
                    {
                        self.config.tasks.record_tool_finished(&task_id);
                    }
                },
            );
            tokio::pin!(process);
            tokio::select! {
                result = &mut process => (result, false),
                () = deadline.expired() => { cancel.cancel(); (process.await, true) }
            }
        };
        if output_truncated {
            output.push_str("\n[external agent output truncated]");
        }
        let output = redactor.redact_string(&output);
        if !output.is_empty() {
            let message = Message {
                role: Role::Assistant,
                blocks: vec![Block::text(&output)],
                ..Message::default()
            };
            if let Err(error) = transcript.append(message).await {
                self.finish(
                    &task_id,
                    TaskStatus::Failed,
                    self.now(),
                    &transcript.messages(),
                    Some(&error.to_string()),
                    Some(&transcript),
                )
                .await;
                return;
            }
        }
        let status = if timed_out {
            TaskStatus::Interrupted
        } else if cancel.is_cancelled() {
            TaskStatus::Canceled
        } else if result
            .as_ref()
            .is_ok_and(|value| value["stopReason"] == "end_turn")
        {
            TaskStatus::Succeeded
        } else {
            TaskStatus::Failed
        };
        let error = if timed_out {
            Some("task deadline exceeded".to_owned())
        } else {
            result.err().or_else(|| {
                (status == TaskStatus::Failed)
                    .then(|| "ACP agent did not complete the turn".to_owned())
            })
        };
        let error = error.map(|message| redactor.redact_string(&message));
        self.finish(
            &task_id,
            status,
            self.now(),
            &transcript.messages(),
            error.as_deref(),
            Some(&transcript),
        )
        .await;
    }
}

/// External agents receive only environment needed for runtime and provider authentication.
pub(super) fn adapter_environment() -> (Vec<String>, Vec<String>) {
    filter_environment(
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))),
    )
}

fn filter_environment(
    entries: impl IntoIterator<Item = (String, String)>,
) -> (Vec<String>, Vec<String>) {
    let mut environment = Vec::new();
    let mut secrets = Vec::new();
    for (key, mut value) in entries {
        let sensitive = key.ends_with("_API_KEY")
            || key.ends_with("_AUTH_TOKEN")
            || key == "CODEX_API_KEY"
            || key == "HTTP_PROXY"
            || key == "HTTPS_PROXY"
            || key == "ALL_PROXY"
            || key == "http_proxy"
            || key == "https_proxy"
            || key == "all_proxy"
            || key.ends_with("_BASE_URL");
        let allowed = sensitive
            || matches!(
                key.as_str(),
                "PATH"
                    | "HOME"
                    | "USER"
                    | "LOGNAME"
                    | "SHELL"
                    | "TMPDIR"
                    | "LANG"
                    | "LC_ALL"
                    | "TERM"
                    | "XDG_CONFIG_HOME"
                    | "XDG_CACHE_HOME"
                    | "XDG_DATA_HOME"
                    | "CODEX_HOME"
                    | "CLAUDE_CONFIG_DIR"
                    | "PI_CODING_AGENT_DIR"
                    | "OPENAI_BASE_URL"
                    | "ANTHROPIC_BASE_URL"
                    | "NO_PROXY"
                    | "no_proxy"
                    | "SSL_CERT_FILE"
                    | "NODE_EXTRA_CA_CERTS"
            );
        if !allowed {
            continue;
        }
        if key == "PATH" {
            value = value
                .split(':')
                .filter(|entry| Path::new(entry).is_absolute())
                .collect::<Vec<_>>()
                .join(":");
            if value.is_empty() {
                continue;
            }
        }
        if sensitive && !value.is_empty() {
            secrets.push(value.clone());
        }
        environment.push(format!("{key}={value}"));
    }
    (environment, secrets)
}

#[cfg(test)]
#[path = "external_tests.rs"]
mod tests;
