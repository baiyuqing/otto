//! The `Tool` adapter for one MCP tool.
//!
//! One `McpTool` wraps one tool advertised by one server's
//! `tools/list` and forwards `execute` to [`crate::mcp::ToolServer::call`].
//!
//! Ownership: an `McpTool` holds an `Arc<dyn ToolServer>` shared with every
//! other tool on the same server, plus its own name, description, schema and
//! redaction secrets, all copied out of the untrusted `ToolInfo` at
//! construction so a later change to the server's advertised list cannot
//! reach an already-built tool. Concurrency: [`Tool::execute`] takes `&self`
//! and calls through to the shared server, which may run concurrently with
//! other tools on it. Security: the tool's name, description, schema and
//! result text are untrusted server data; the name is sanitized and length
//! capped before it reaches the model, and the result text is redacted and
//! capped the same way the built-in tools are.

use std::collections::HashMap;
use std::sync::Arc;

use otto_core::model::{
    EffectCertainty, OperationDisposition, OperationOutcome, OperationStopReason, ToolDefinition,
};
use otto_core::safetext::dynamic_redaction_marker;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::{capped_text_result, redact_exact_text};
use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};
use crate::mcp::{BearerSource, CallError, CallOutcome, ContentBlock, ToolInfo, ToolServer};

/// Every registered MCP tool name starts with this.
pub(crate) const NAME_PREFIX: &str = "mcp__";
/// `otto_core::tool::safe_prompt_tool_name` rejects anything longer.
pub(crate) const MAX_TOOL_NAME_BYTES: usize = 64;
/// The advertised description is cut to this many bytes, on a char boundary.
pub(crate) const MAX_DESCRIPTION_BYTES: usize = 1024;

/// The fallback schema for a tool that advertises no usable `inputSchema`.
const FALLBACK_SCHEMA: &str = r#"{"type":"object"}"#;

/// Builds `mcp__<server>__<tool>`, sanitizing every byte of `tool` outside
/// `[A-Za-z0-9_-]` to `_`. Returns `None` when the result would exceed
/// [`MAX_TOOL_NAME_BYTES`]; truncating is not attempted because two different
/// tool names could then truncate to the same prefixed name.
pub(crate) fn prefixed_name(server: &str, tool: &str) -> Option<String> {
    let sanitized: String = tool
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                byte as char
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{NAME_PREFIX}{server}__{sanitized}");
    if name.len() > MAX_TOOL_NAME_BYTES {
        None
    } else {
        Some(name)
    }
}

/// Cuts `text` to at most `limit` bytes, on a char boundary.
fn truncate_on_char_boundary(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// `[<server>] ` followed by the tool's description, falling back to its
/// title and then its remote name, capped at [`MAX_DESCRIPTION_BYTES`].
fn build_description(server: &str, info: &ToolInfo) -> String {
    let body = info
        .description
        .as_deref()
        .filter(|text| !text.is_empty())
        .or_else(|| info.title.as_deref().filter(|text| !text.is_empty()))
        .unwrap_or(&info.name);
    truncate_on_char_boundary(&format!("[{server}] {body}"), MAX_DESCRIPTION_BYTES)
}

/// The server's `inputSchema` verbatim when it is a JSON object, else the
/// fallback `{"type": "object"}`.
fn build_parameters(info: &ToolInfo) -> String {
    match &info.input_schema {
        Some(value) if value.is_object() => value.to_string(),
        _ => FALLBACK_SCHEMA.to_owned(),
    }
}

/// One MCP tool, adapted to the `Tool` contract.
pub(crate) struct McpTool {
    server: Arc<dyn ToolServer>,
    name: String,
    remote_name: String,
    description: String,
    parameters: String,
    max_output_bytes: usize,
    secrets: Vec<String>,
    marker: String,
    /// The OAuth token source for this server, when `auth = "oauth"`. Kept so
    /// [`Self::effective_secrets`] can read the current access/refresh tokens
    /// at call time rather than the tokens present when the tool was built,
    /// covering a token rotated by a mid-session refresh.
    bearer: Option<Arc<dyn BearerSource>>,
}

impl McpTool {
    pub(crate) fn new(
        server: Arc<dyn ToolServer>,
        name: String,
        info: &ToolInfo,
        max_output_bytes: usize,
        secrets: Vec<String>,
        bearer: Option<Arc<dyn BearerSource>>,
    ) -> Self {
        let description = build_description(server.name(), info);
        let parameters = build_parameters(info);
        let marker = dynamic_redaction_marker(&secrets).unwrap_or_default();
        Self {
            server,
            name,
            remote_name: info.name.clone(),
            description,
            parameters,
            max_output_bytes,
            secrets,
            marker,
            bearer,
        }
    }

    fn server_name(&self) -> &str {
        self.server.name()
    }

    /// The tool's name as advertised by the server, before prefixing. Used by
    /// `crate::runtime::wiring`'s cross-server collision warning, which needs the
    /// remote name for its message even though the registered name is
    /// [`Self::name`]'s sanitized, server-prefixed form.
    pub(crate) fn remote_name(&self) -> &str {
        &self.remote_name
    }

    /// `self.secrets` plus the OAuth bearer's current access/refresh tokens,
    /// deduplicated, when this tool's server uses OAuth. Reads `bearer` at
    /// call time so a token refreshed mid-session is still redacted.
    fn effective_secrets(&self) -> Vec<String> {
        let Some(bearer) = &self.bearer else {
            return self.secrets.clone();
        };
        let mut combined = self.secrets.clone();
        for secret in bearer.secrets() {
            if !secret.is_empty() && !combined.contains(&secret) {
                combined.push(secret);
            }
        }
        combined
    }

    /// Parses the call arguments, mapping the shapes the model produces for
    /// "no arguments" to an empty object; any other non-object is rejected.
    fn parse_arguments(&self, arguments: &RawValue) -> Result<Value, ToolResult> {
        let raw = arguments.get().trim();
        if raw.is_empty() || raw == "null" {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        match serde_json::from_str::<Value>(raw) {
            Ok(value) if value.is_object() => Ok(value),
            _ => Err(error_result(format!(
                "mcp {}: arguments must be a JSON object",
                self.server_name()
            ))
            .not_started()),
        }
    }

    /// Redacts `text` against this tool's secrets, plus the OAuth bearer's
    /// current tokens when it has one. Used for both call results and the
    /// formatted error text: server-controlled error messages (RPC error
    /// text, a stdio child's stderr tail) can echo a secret just as a result
    /// can.
    fn redact(&self, text: &str) -> String {
        if self.bearer.is_some() {
            let secrets = self.effective_secrets();
            match dynamic_redaction_marker(&secrets) {
                Some(marker) => redact_exact_text(text, &secrets, &marker),
                // ponytail: the bearer's tokens pushed the combined secrets
                // past the redaction limits (huge access/refresh token).
                // Fall back to the static secrets, already known safe from
                // the connect-time check in `runtime::wiring::connect_mcp`,
                // rather than blanking the whole result.
                None => redact_exact_text(text, &self.secrets, &self.marker),
            }
        } else {
            redact_exact_text(text, &self.secrets, &self.marker)
        }
    }

    fn map_outcome(&self, outcome: CallOutcome) -> ToolResult {
        let text = render_outcome_text(&outcome, self.server_name());
        let redacted = self.redact(&text);
        let mut result = capped_text_result(&redacted, self.max_output_bytes);
        result.is_error = outcome.is_error;
        result
    }

    fn map_error(&self, error: CallError) -> ToolResult {
        let server = self.server_name();
        match error {
            CallError::Cancelled => error_result(CONTEXT_CANCELED).cancelled_unknown(),
            CallError::Timeout => {
                error_result(format!("mcp {server}: timed out")).deadline_unknown()
            }
            CallError::Interrupted => {
                let mut result = error_result(format!("mcp {server}: {error}"));
                result.outcome_override = Some(OperationOutcome {
                    disposition: OperationDisposition::Interrupted,
                    effect_certainty: EffectCertainty::Unknown,
                    stop_reason: Some(OperationStopReason::TransportLost),
                });
                result
            }
            CallError::PluginUnauthorized => {
                error_result(format!("mcp {server}: {error}")).not_started()
            }
            CallError::Stopping => error_result(format!("mcp {server}: {error}")).not_started(),
            CallError::Rpc { .. } => error_result(self.redact(&format!("mcp {server}: {error}"))),
            CallError::Transport(_) => {
                error_result(self.redact(&format!("mcp {server}: {error}"))).transport_unknown()
            }
            CallError::NeedsLogin => error_result(format!(
                "mcp {server}: authorization required; run 'otto mcp login {server}'"
            ))
            .not_started(),
        }
    }
}

/// Renders a `tools/call` outcome as the text the model sees, following the
/// design's "Result mapping" section.
fn render_outcome_text(outcome: &CallOutcome, server: &str) -> String {
    if !outcome.content.is_empty() {
        outcome
            .content
            .iter()
            .map(render_block)
            .collect::<Vec<_>>()
            .join("\n")
    } else if let Some(structured) = &outcome.structured_content {
        serde_json::to_string(structured).unwrap_or_default()
    } else if outcome.is_error {
        format!("mcp {server}: tool reported an error")
    } else {
        String::new()
    }
}

fn render_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Image {
            mime_type,
            data_len,
        } => {
            format!("[image {mime_type}, {data_len} bytes base64 omitted]")
        }
        ContentBlock::Audio {
            mime_type,
            data_len,
        } => {
            format!("[audio {mime_type}, {data_len} bytes base64 omitted]")
        }
        ContentBlock::ResourceLink { uri, name, title } => {
            let label = title
                .as_deref()
                .filter(|text| !text.is_empty())
                .unwrap_or(name);
            format!("[resource {uri}] {label}")
        }
        ContentBlock::Resource {
            uri,
            mime_type,
            text,
            blob_len,
        } => match text {
            Some(text) => text.clone(),
            None => {
                let mime = mime_type.as_deref().unwrap_or("unknown");
                format!("[resource {uri}, {mime}, {blob_len} bytes blob omitted]")
            }
        },
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        otto_core::model::RetrySafety::NonIdempotent
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: Some(
                RawValue::from_string(self.parameters.clone()).expect("schema is valid JSON"),
            ),
        }
    }

    fn approval_spec(
        &self,
        arguments: &RawValue,
    ) -> Result<Option<super::ApprovalSpec>, ToolResult> {
        let details=serde_json::json!({"tool":self.name,"arguments":serde_json::from_str::<serde_json::Value>(arguments.get()).map_err(|_|error_result("invalid MCP arguments"))?}).to_string();
        if self.redact(&details) != details {
            return Err(error_result("MCP approval would expose credentials").not_started());
        }
        Ok(Some(super::ApprovalSpec {
            version: crate::approval::digest(arguments.get()),
            source: "mcp".into(),
            authority: otto_core::approval::Authority::Boundary,
            title: format!("Authorize {}?", self.name),
            reason: "This external tool can affect an external system.".into(),
            persistent: false,
            details: serde_json::json!({
                "tool":self.name,
                "arguments":serde_json::from_str::<serde_json::Value>(arguments.get())
                    .map_err(|_| error_result("invalid MCP arguments"))?,
                "untrusted_tool_description":self.description
            })
            .to_string(),
        }))
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED).cancelled_not_started();
        }
        let args = match self.parse_arguments(arguments) {
            Ok(args) => args,
            Err(result) => return result,
        };
        match self.server.call(&self.remote_name, args, cancel).await {
            Ok(outcome) => self.map_outcome(outcome),
            Err(error) => self.map_error(error),
        }
    }
}

/// A compact catalog/search entry for one already-connected MCP tool.
#[derive(Clone)]
pub(crate) struct McpCatalogEntry {
    pub server: Arc<dyn ToolServer>,
    pub info: ToolInfo,
    pub prefixed_name: String,
    pub max_output_bytes: usize,
    pub secrets: Vec<String>,
    pub bearer: Option<Arc<dyn BearerSource>>,
}

/// Builds the two lazy MCP router tools. They are intentionally tiny compared
/// with large server schemas: the model searches names/descriptions, then calls
/// one selected MCP tool by its already-prefixed name.
pub(crate) fn router_tools(
    entries: Vec<McpCatalogEntry>,
    max_output_bytes: usize,
) -> Vec<Box<dyn Tool + Send + Sync>> {
    if entries.is_empty() {
        return Vec::new();
    }
    let catalog = Arc::new(McpCatalog::new(entries));
    vec![
        Box::new(McpSearchTools::new(Arc::clone(&catalog), max_output_bytes)),
        Box::new(McpCallTool::new(catalog)),
    ]
}

struct McpCatalog {
    entries: Vec<McpCatalogEntry>,
    by_name: HashMap<String, usize>,
}

impl McpCatalog {
    fn new(entries: Vec<McpCatalogEntry>) -> Self {
        let by_name = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.prefixed_name.clone(), index))
            .collect();
        Self { entries, by_name }
    }

    fn get(&self, name: &str) -> Option<&McpCatalogEntry> {
        self.by_name.get(name).map(|&index| &self.entries[index])
    }
}

struct McpSearchTools {
    catalog: Arc<McpCatalog>,
    max_output_bytes: usize,
}

impl McpSearchTools {
    fn new(catalog: Arc<McpCatalog>, max_output_bytes: usize) -> Self {
        Self {
            catalog,
            max_output_bytes,
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
}

pub(crate) const MCP_SEARCH_TOOLS_RETRY: (&str, otto_core::model::RetrySafety) = (
    "mcp_search_tools",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait::async_trait]
impl Tool for McpSearchTools {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        MCP_SEARCH_TOOLS_RETRY.1
    }
    fn definition(&self) -> ToolDefinition {
        definition(
            "mcp_search_tools",
            "Search connected MCP tools by name or description. Use this before \
                calling mcp_call_tool when you need an MCP server such as Notion. \
                Returns compact tool names and descriptions, not full schemas.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Keywords for the MCP tool or server you need, such as 'notion search' or 'github issue'."}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        let args: SearchArgs = match serde_json::from_str(arguments.get()) {
            Ok(args) => args,
            Err(error) => {
                return error_result(format!("mcp_search_tools: invalid arguments: {error}"))
                    .not_started();
            }
        };
        let terms: Vec<String> = args
            .query
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .collect();
        let mut lines = Vec::new();
        for entry in &self.catalog.entries {
            let description = build_description(entry.server.name(), &entry.info);
            let haystack = format!(
                "{} {} {} {}",
                entry.prefixed_name,
                entry.server.name(),
                entry.info.name,
                description
            )
            .to_ascii_lowercase();
            if terms.is_empty() || terms.iter().all(|term| haystack.contains(term)) {
                lines.push(format!("{} — {}", entry.prefixed_name, description));
            }
        }
        if lines.is_empty() {
            return text_result("no matching MCP tools");
        }
        capped_text_result(&lines.join("\n"), self.max_output_bytes)
    }
}

struct McpCallTool {
    catalog: Arc<McpCatalog>,
}

impl McpCallTool {
    fn new(catalog: Arc<McpCatalog>) -> Self {
        Self { catalog }
    }
}

#[derive(Deserialize)]
struct CallArgs {
    tool: String,
    #[serde(default)]
    arguments: Option<Value>,
}

pub(crate) const MCP_CALL_TOOL_RETRY: (&str, otto_core::model::RetrySafety) = (
    "mcp_call_tool",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait::async_trait]
impl Tool for McpCallTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        MCP_CALL_TOOL_RETRY.1
    }
    fn definition(&self) -> ToolDefinition {
        definition(
            "mcp_call_tool",
            "Call one connected MCP tool by the full name returned from \
                mcp_search_tools. Provide arguments as a JSON object. This keeps large \
                MCP tool schemas out of the main context until a concrete call is \
                needed.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "description": "Full MCP tool name, for example mcp__notion__notion-fetch."},
                    "arguments": {"type": "object", "description": "Arguments for the remote MCP tool."}
                },
                "required": ["tool"],
                "additionalProperties": false
            }),
        )
    }

    fn approval_spec(
        &self,
        arguments: &RawValue,
    ) -> Result<Option<super::ApprovalSpec>, ToolResult> {
        let (tool, raw) = self.resolve(arguments)?;
        tool.approval_spec(&raw)
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        match self.resolve(arguments) {
            Ok((tool, raw)) => tool.execute(&raw, cancel).await,
            Err(result) => result,
        }
    }
}

impl McpCallTool {
    /// Resolves the selected catalog entry into the concrete [`McpTool`] and its
    /// inner arguments. Approval and execution both go through this, so the
    /// call that is approved is the call that runs.
    fn resolve(&self, arguments: &RawValue) -> Result<(McpTool, Box<RawValue>), ToolResult> {
        let args: CallArgs = serde_json::from_str(arguments.get()).map_err(|error| {
            error_result(format!("mcp_call_tool: invalid arguments: {error}")).not_started()
        })?;
        let Some(entry) = self.catalog.get(&args.tool) else {
            return Err(
                error_result(format!("mcp_call_tool: unknown MCP tool {:?}", args.tool))
                    .not_started(),
            );
        };
        let call_args = args
            .arguments
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if !call_args.is_object() {
            return Err(
                error_result("mcp_call_tool: arguments must be a JSON object").not_started(),
            );
        }
        let tool = McpTool::new(
            Arc::clone(&entry.server),
            entry.prefixed_name.clone(),
            &entry.info,
            entry.max_output_bytes,
            entry.secrets.clone(),
            entry.bearer.clone(),
        );
        let raw = RawValue::from_string(call_args.to_string()).expect("JSON value is valid");
        Ok((tool, raw))
    }
}

/// Skips, with one warning each, a tool whose prefixed name would exceed
/// [`MAX_TOOL_NAME_BYTES`], and both tools of a same-name collision after
/// sanitizing.
pub(crate) fn tools_for(
    server: Arc<dyn ToolServer>,
    tools: &[ToolInfo],
    max_output_bytes: usize,
    secrets: Vec<String>,
    bearer: Option<Arc<dyn BearerSource>>,
) -> (Vec<McpTool>, Vec<String>) {
    let server_name = server.name().to_owned();
    let mut warnings = Vec::new();
    let mut names: Vec<Option<String>> = Vec::with_capacity(tools.len());
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for info in tools {
        match prefixed_name(&server_name, &info.name) {
            Some(name) => {
                *counts.entry(name.clone()).or_insert(0) += 1;
                names.push(Some(name));
            }
            None => {
                warnings.push(format!(
                    "mcp {server_name}: skipping tool {:?}: name exceeds {MAX_TOOL_NAME_BYTES} bytes after prefixing",
                    info.name
                ));
                names.push(None);
            }
        }
    }

    let mut built = Vec::with_capacity(tools.len());
    for (info, name) in tools.iter().zip(names) {
        let Some(name) = name else { continue };
        if counts[&name] > 1 {
            warnings.push(format!(
                "mcp {server_name}: skipping tool {:?}: sanitized name {name:?} collides with another tool on this server",
                info.name
            ));
            continue;
        }
        built.push(McpTool::new(
            Arc::clone(&server),
            name,
            info,
            max_output_bytes,
            secrets.clone(),
            bearer.clone(),
        ));
    }
    (built, warnings)
}

#[cfg(test)]
mod tests;
