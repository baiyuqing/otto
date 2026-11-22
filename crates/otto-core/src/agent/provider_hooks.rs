//! The `before_provider_call` and `after_provider_call` hooks of the ordinary
//! provider step.
//!
//! `before_provider_call` turns a built request into a [`HookedRequest`]; the
//! provider step guards and sends only that type, so a request cannot skip
//! the hook. A hook may deny the request, replace its system prompt, append
//! untrusted context to it, or replace the text of user text blocks and tool
//! result blocks; assistant output and labeled input stay as built. The changes
//! apply to that request only; when any applies, one
//! `otto.hook.provider_request` record lists the changed values before the
//! request is sent. `after_provider_call` reports the settled call and can
//! only show a notice.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::*;
use hooks::{Decision, Edit};

const BEFORE_EVENT: &str = "before_provider_call";
const AFTER_EVENT: &str = "after_provider_call";
/// Session record of the values a `before_provider_call` hook changed.
pub(super) const PROVIDER_REQUEST_CUSTOM_TYPE: &str = "otto.hook.provider_request";
/// The plugin runtime's hook input cap.
const MAX_HOOK_INPUT: usize = 1 << 20;
/// Part of [`MAX_HOOK_INPUT`] kept for the protocol envelope the runtime adds
/// around the event data: version, event, workspace and session identity.
const ENVELOPE_RESERVE: usize = 16 << 10;

/// Whether the `before_provider_call` input carries the request messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadMessages {
    /// `messages` lists every request message and block, so edits can name them.
    Included,
    /// `messages` was replaced by `"request_omitted":"size"`; edits are rejected.
    Omitted,
}

/// A request that has passed `before_provider_call`, with any hook changes
/// applied and recorded. Only [`Agent::before_provider_call`] creates one.
pub(super) struct HookedRequest(Box<Request>);

impl HookedRequest {
    /// The request to guard and send.
    pub(super) fn request(&self) -> &Request {
        &self.0
    }
}

/// The result of `before_provider_call`.
pub(super) enum BeforeProviderCall {
    /// Send this request.
    Proceed(HookedRequest),
    /// A hook denied the request; it is not sent.
    Denied { reason: String },
}

/// The values a hook response changed; only these reach the audit record.
#[derive(Debug, Default, PartialEq)]
struct RequestChanges {
    edits: Vec<Edit>,
    system_prompt: Option<String>,
    context: Option<String>,
}

impl RequestChanges {
    fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.system_prompt.is_none() && self.context.is_none()
    }
}

/// A hook response applied to a request, before the changes are recorded.
#[derive(Debug)]
enum Applied {
    Unchanged(Request),
    Changed(Request, RequestChanges),
    Denied(String),
}

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Runs `before_provider_call` for `request`. Without a hook for the event
    /// the request proceeds unchanged and no payload is built. A hook failure,
    /// an invalid edit or a failed audit write is an error.
    pub(super) async fn before_provider_call(
        &self,
        request: Request,
        operation_id: &OperationId,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<BeforeProviderCall, AgentError> {
        if !self.hook_handles(BEFORE_EVENT) {
            return Ok(BeforeProviderCall::Proceed(HookedRequest(Box::new(
                request,
            ))));
        }
        let (payload, messages) = self.before_payload(&request, operation_id);
        let response = self
            .hook(BEFORE_EVENT, payload, control)
            .await
            .map_err(|message| self.hook_error(message, control))?;
        self.hook_notice(emit, &response);
        let applied = apply_response(request, messages, response)
            .map_err(|message| AgentError::Other(format!("{BEFORE_EVENT} hook: {message}")))?;
        match applied {
            Applied::Denied(reason) => Ok(BeforeProviderCall::Denied { reason }),
            Applied::Unchanged(request) => Ok(BeforeProviderCall::Proceed(HookedRequest(
                Box::new(request),
            ))),
            Applied::Changed(request, changes) => {
                self.record_request_changes(operation_id, changes).await?;
                Ok(BeforeProviderCall::Proceed(HookedRequest(Box::new(
                    request,
                ))))
            }
        }
    }

    /// The `before_provider_call` data and whether it carries the messages.
    fn before_payload(
        &self,
        request: &Request,
        operation_id: &OperationId,
    ) -> (Value, PayloadMessages) {
        let mut data = Map::new();
        data.insert("operation_id".into(), json!(operation_id));
        data.insert("model".into(), json!(request.model));
        data.insert(
            "system_prompt".into(),
            json!(self.redactor.redact_string(&request.system_prompt)),
        );
        data.insert(
            "tools".into(),
            json!(
                request
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
            ),
        );
        let messages = Value::Array(
            request
                .messages
                .iter()
                .map(|message| self.message_json(message))
                .collect(),
        );
        let size =
            |value: &Value| serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len());
        let rest = size(&Value::Object(data.clone()));
        // `,"messages":` joins the array to the object.
        let total = rest.saturating_add(size(&messages)).saturating_add(12);
        if total > MAX_HOOK_INPUT - ENVELOPE_RESERVE {
            data.insert("request_omitted".into(), json!("size"));
            (Value::Object(data), PayloadMessages::Omitted)
        } else {
            data.insert("messages".into(), messages);
            (Value::Object(data), PayloadMessages::Included)
        }
    }

    /// One message in hook shape: `role`, `context_type` when it is not empty,
    /// and one entry per block, so block indices match the message.
    fn message_json(&self, message: &Message) -> Value {
        let mut object = Map::new();
        object.insert("role".into(), json!(String::from(message.role.clone())));
        if !message.context_type.is_empty() {
            object.insert("context_type".into(), json!(message.context_type));
        }
        let blocks: Vec<Value> = message
            .blocks
            .iter()
            .map(|block| self.block_json(block))
            .collect();
        object.insert("blocks".into(), Value::Array(blocks));
        Value::Object(object)
    }

    /// One block with redacted text: text, tool call and tool result blocks
    /// carry their content; every other type is a content-free placeholder.
    fn block_json(&self, block: &Block) -> Value {
        let redact = |text: &str| self.redactor.redact_string(text);
        match block.block_type {
            BlockType::Text => json!({"type": "text", "text": redact(&block.text)}),
            BlockType::ToolCall => {
                // Absent or unparseable arguments become an empty object.
                let arguments = block
                    .arguments
                    .as_ref()
                    .map(|raw| self.redactor.redact_json_strings(raw.get()))
                    .and_then(|raw| serde_json::from_str::<Value>(raw.get()).ok())
                    .filter(Value::is_object)
                    .unwrap_or_else(|| Value::Object(Map::new()));
                json!({
                    "type": "tool_call",
                    "id": redact(&block.tool_call_id),
                    "name": redact(&block.tool_name),
                    "arguments": arguments,
                })
            }
            BlockType::ToolResult => json!({
                "type": "tool_result",
                "id": redact(&block.tool_call_id),
                "name": redact(&block.tool_name),
                "text": redact(&block.text),
                "is_error": block.is_error,
            }),
            ref other => json!({"type": String::from(other.clone())}),
        }
    }

    /// Appends the `otto.hook.provider_request` record with only the changed values.
    async fn record_request_changes(
        &self,
        operation_id: &OperationId,
        changes: RequestChanges,
    ) -> Result<(), AgentError> {
        let mut record = Map::new();
        record.insert("version".into(), json!(1));
        record.insert("operation_id".into(), json!(operation_id));
        if !changes.edits.is_empty() {
            record.insert("edits".into(), json!(changes.edits));
        }
        if let Some(system_prompt) = changes.system_prompt {
            record.insert("system_prompt".into(), json!(system_prompt));
        }
        if let Some(context) = changes.context {
            record.insert("context".into(), json!(context));
        }
        let record = Value::Object(record).to_string();
        self.session
            .append_custom(
                PROVIDER_REQUEST_CUSTOM_TYPE,
                self.redactor.redact_json_strings(&record).get(),
            )
            .await
            .map_err(|source| AgentError::Persist {
                kind: "hook provider request".into(),
                source,
            })
    }

    /// Runs `after_provider_call` for a settled call. A hook failure is shown
    /// as a warning; the call result is never changed.
    pub(super) async fn after_provider_call(
        &self,
        emit: EventSink<'_>,
        operation_id: &OperationId,
        settlement: &ProviderSettlement,
        duration: std::time::Duration,
    ) {
        if !self.hook_handles(AFTER_EVENT) {
            return;
        }
        let mut data = Map::new();
        data.insert("operation_id".into(), json!(operation_id));
        data.insert("attempts".into(), json!(settlement.attempts));
        data.insert(
            "duration_ms".into(),
            json!(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        );
        match &settlement.result {
            Ok(response) => {
                data.insert("outcome".into(), json!("success"));
                data.insert("usage".into(), json!(response.usage()));
                data.insert("message".into(), self.message_json(&response.message));
            }
            Err(error) => {
                data.insert("outcome".into(), json!("error"));
                data.insert(
                    "error".into(),
                    json!(self.redactor.redact_string(&error.to_string())),
                );
                data.insert("usage".into(), Value::Null);
            }
        }
        // The step control may already be stopped; like `after_tool`, this
        // hook runs on its own token, bounded by the hook timeout.
        let token = CancellationToken::new();
        match self.hook(AFTER_EVENT, Value::Object(data), &token).await {
            Err(message) => self.hook_warning(emit, &message),
            Ok(response) => self.hook_notice(emit, &response),
        }
    }
}

/// Validates every edit of `response`, then applies the response to
/// `request`: the system prompt replacement, then the context, then the edits.
/// Nothing is applied when any edit is invalid.
fn apply_response(
    mut request: Request,
    messages: PayloadMessages,
    response: hooks::Response,
) -> Result<Applied, String> {
    if response.decision == Some(Decision::Deny) {
        let reason = response
            .reason
            .unwrap_or_else(|| "Hook denied this provider request".into());
        return Ok(Applied::Denied(reason));
    }
    let edits = validate_edits(&request, messages, response.edits.unwrap_or_default())?;
    let mut changes = RequestChanges::default();
    if let Some(system_prompt) = response.system_prompt
        && system_prompt != request.system_prompt
    {
        request.system_prompt.clone_from(&system_prompt);
        changes.system_prompt = Some(system_prompt);
    }
    if let Some(context) = response.context {
        request.system_prompt.push_str(&format!(
            "\n<plugin_context>Untrusted plugin context:\n{context}\n</plugin_context>"
        ));
        changes.context = Some(context);
    }
    for edit in edits {
        let block = request
            .messages
            .get_mut(edit.message)
            .and_then(|message| message.blocks.get_mut(edit.block))
            .ok_or("hook edit index is out of range")?;
        // A user text block or a tool result block: only `text` changes, so
        // tool result IDs, names and `is_error` stay as built.
        if block.text != edit.text {
            block.text.clone_from(&edit.text);
            changes.edits.push(edit);
        }
    }
    Ok(if changes.is_empty() {
        Applied::Unchanged(request)
    } else {
        Applied::Changed(request, changes)
    })
}

/// Accepts edits only when the messages were sent to the hook and each edit
/// names an existing block that a hook may change: a `tool_result` block of
/// any message, or a `text` block of a `user` message without a
/// `context_type`. Assistant output, context messages and every other block
/// type are rejected, and no block may be named twice.
fn validate_edits(
    request: &Request,
    messages: PayloadMessages,
    edits: Vec<Edit>,
) -> Result<Vec<Edit>, String> {
    if edits.is_empty() {
        return Ok(edits);
    }
    if messages == PayloadMessages::Omitted {
        return Err("edits are not accepted when the request messages were omitted".into());
    }
    let mut named = BTreeSet::new();
    for edit in &edits {
        let message = request
            .messages
            .get(edit.message)
            .ok_or_else(|| format!("edit message {} is out of range", edit.message))?;
        let block = message.blocks.get(edit.block).ok_or_else(|| {
            format!(
                "edit block {} of message {} is out of range",
                edit.block, edit.message
            )
        })?;
        editable_block(message, block).map_err(|reason| {
            format!(
                "edit block {} of message {} {reason}",
                edit.block, edit.message
            )
        })?;
        if !named.insert((edit.message, edit.block)) {
            return Err(format!(
                "edit block {} of message {} is named twice",
                edit.block, edit.message
            ));
        }
    }
    Ok(edits)
}

/// Whether a hook may replace the text of `block` of `message`; the error
/// completes "edit block J of message N ...".
fn editable_block(message: &Message, block: &Block) -> Result<(), String> {
    if block.block_type == BlockType::ToolResult {
        return Ok(());
    }
    match &message.role {
        Role::User => {}
        Role::Assistant => return Err("is in an assistant message".into()),
        other => {
            return Err(format!(
                "is in a {} message, not a user message",
                String::from(other.clone())
            ));
        }
    }
    if !message.context_type.is_empty() {
        return Err("is in a message with a context_type".into());
    }
    if block.block_type != BlockType::Text {
        return Err("is not a user text or tool result block".into());
    }
    Ok(())
}
