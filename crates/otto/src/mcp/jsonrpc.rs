//! JSON-RPC 2.0 message types and the modern `_meta` builder.
//!
//! Owned by the MCP codec step; see `docs/specs/2026-09-19-mcp-design.md`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::mcp::{CallOutcome, ContentBlock, Era, LEGACY_VERSION, MODERN_VERSION, ToolInfo};

/// Request ids this client allocates are integers; incoming ids may be number, string or null.
pub type RequestId = Value;

/// A JSON-RPC request as sent to the server.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    jsonrpc: &'static str,
    id: RequestId,
    method: String,
    params: Value,
}

impl Request {
    /// Build a request with the given id, method, and parameters.
    pub fn new(id: i64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: json!(id),
            method: method.to_string(),
            params,
        }
    }
}

/// A JSON-RPC notification as sent to the server (no response expected).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notification {
    jsonrpc: &'static str,
    method: String,
    #[serde(skip_serializing_if = "is_null")]
    params: Value,
}

fn is_null(v: &Value) -> bool {
    v.is_null()
}

impl Notification {
    /// Build a notification with the given method and parameters.
    /// Params are omitted from serialization if null.
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        }
    }
}

/// An incoming JSON-RPC message. Batches and messages with both result and error are rejected.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response {
        id: RequestId,
        result: Value,
    },
    Error {
        id: RequestId,
        error: RpcError,
    },
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
}

/// A JSON-RPC error object as the server sent it. Untrusted data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Error codes defined by JSON-RPC 2.0 spec extensions.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32020;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// Parse a single JSON-RPC message text. Rejects batches, missing jsonrpc: "2.0", and messages with both result and error.
pub fn parse_incoming(text: &str) -> Result<Incoming, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid json: {}", e))?;

    // Reject batches (arrays)
    if value.is_array() {
        return Err("batch requests not supported".to_string());
    }

    let obj = value
        .as_object()
        .ok_or_else(|| "message must be an object".to_string())?;

    // Check jsonrpc field
    if obj.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Err("missing or invalid jsonrpc: expected \"2.0\"".to_string());
    }

    // Reject if both result and error are present
    if obj.contains_key("result") && obj.contains_key("error") {
        return Err("message must not have both result and error".to_string());
    }

    let id = obj.get("id").cloned();
    let method = obj
        .get("method")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if let Some(result) = obj.get("result") {
        // Response with result
        let id = id.ok_or_else(|| "response missing id".to_string())?;
        Ok(Incoming::Response {
            id,
            result: result.clone(),
        })
    } else if let Some(error) = obj.get("error") {
        // Response with error
        let id = id.ok_or_else(|| "error response missing id".to_string())?;
        let error: RpcError =
            serde_json::from_value(error.clone()).map_err(|e| format!("invalid error: {}", e))?;
        Ok(Incoming::Error { id, error })
    } else if let Some(method) = method {
        if let Some(id) = id {
            // Request (has id)
            Ok(Incoming::Request {
                id,
                method,
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            })
        } else {
            // Notification (no id)
            Ok(Incoming::Notification {
                method,
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            })
        }
    } else {
        Err("message missing method or result/error".to_string())
    }
}

/// Insert modern protocol metadata into params. Returns params if already empty, or merges into existing _meta.
pub fn modern_meta(params: Value) -> Value {
    let mut obj = if params.is_null() {
        serde_json::Map::new()
    } else {
        params.as_object().cloned().unwrap_or_default()
    };

    let mut meta = obj
        .get("_meta")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    meta.insert(
        "io.modelcontextprotocol/protocolVersion".to_string(),
        json!(MODERN_VERSION),
    );
    meta.insert(
        "io.modelcontextprotocol/clientCapabilities".to_string(),
        json!({}),
    );
    meta.insert(
        "io.modelcontextprotocol/clientInfo".to_string(),
        json!({
            "name": "otto",
            "version": env!("CARGO_PKG_VERSION")
        }),
    );

    obj.insert("_meta".to_string(), json!(meta));
    Value::Object(obj)
}

/// Build the initialize request parameters for the legacy protocol version.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": LEGACY_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "otto",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// Decode the initialize response to extract the server's protocolVersion.
pub fn decode_initialize(result: &Value) -> Result<String, String> {
    let obj = result
        .as_object()
        .ok_or_else(|| "initialize result must be an object".to_string())?;

    let version = obj
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing or invalid protocolVersion".to_string())?;

    if version.is_empty() {
        Err("protocolVersion must not be empty".to_string())
    } else {
        Ok(version.to_string())
    }
}

/// Decode a tools/list response into a vector of ToolInfo and optional nextCursor.
/// Entries that fail to decode are skipped (not fatal); unknown fields are ignored.
pub fn decode_tools_list(result: &Value) -> Result<(Vec<ToolInfo>, Option<String>), String> {
    let obj = result
        .as_object()
        .ok_or_else(|| "tools/list result must be an object".to_string())?;

    let tools_array = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing or invalid tools array".to_string())?;

    let mut tools = Vec::new();
    for tool_val in tools_array {
        if let Ok(tool) = serde_json::from_value::<ToolInfo>(tool_val.clone()) {
            tools.push(tool);
        }
        // Skip entries that fail to decode; not fatal
    }

    let next_cursor = obj
        .get("nextCursor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Ok((tools, next_cursor))
}

/// Decodes a `tools/call` result. Blocks of an unknown type, or missing a
/// required field, are skipped. On a modern server a `resultType` other than
/// `"complete"` is an error.
pub fn decode_call_result(result: &Value, era: &Era) -> Result<CallOutcome, String> {
    let obj = result
        .as_object()
        .ok_or_else(|| "call result must be an object".to_string())?;
    if *era == Era::Modern
        && let Some(result_type) = obj.get("resultType").and_then(Value::as_str)
        && result_type != "complete"
    {
        return Err(format!("unsupported resultType {result_type}"));
    }
    let content = obj
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().filter_map(decode_block).collect())
        .unwrap_or_default();
    Ok(CallOutcome {
        content,
        structured_content: obj.get("structuredContent").cloned(),
        is_error: obj.get("isError").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn decode_block(block: &Value) -> Option<ContentBlock> {
    let string = |object: &Value, key: &str| object.get(key)?.as_str().map(str::to_owned);
    Some(match block.get("type")?.as_str()? {
        "text" => ContentBlock::Text {
            text: string(block, "text")?,
        },
        "image" => ContentBlock::Image {
            mime_type: string(block, "mimeType")?,
            data_len: string(block, "data")?.len(),
        },
        "audio" => ContentBlock::Audio {
            mime_type: string(block, "mimeType")?,
            data_len: string(block, "data")?.len(),
        },
        "resource_link" => ContentBlock::ResourceLink {
            uri: string(block, "uri")?,
            name: string(block, "name")?,
            title: string(block, "title"),
        },
        "resource" => {
            let resource = block.get("resource")?;
            ContentBlock::Resource {
                uri: string(resource, "uri")?,
                mime_type: string(resource, "mimeType"),
                text: string(resource, "text"),
                blob_len: string(resource, "blob").map_or(0, |blob| blob.len()),
            }
        }
        _ => return None,
    })
}

/// Check if an RpcError indicates the server does not support the modern protocol version.
pub fn supports_modern(error: &RpcError) -> bool {
    error.code == UNSUPPORTED_PROTOCOL_VERSION
        && error
            .data
            .as_ref()
            .and_then(|d| d.get("supported"))
            .and_then(|s| s.as_array())
            .map(|arr| arr.iter().any(|v| v.as_str() == Some(MODERN_VERSION)))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = Request::new(1, "tools/list", json!({}));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"tools/list\""));
    }

    #[test]
    fn test_notification_serialization_with_params() {
        let notif = Notification::new("notifications/cancelled", json!({"requestId": 42}));
        let json = serde_json::to_string(&notif).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"notifications/cancelled\""));
        assert!(json.contains("\"params\""));
    }

    #[test]
    fn test_notification_serialization_without_params() {
        let notif = Notification::new("some_method", Value::Null);
        let json = serde_json::to_string(&notif).unwrap();
        assert!(!json.contains("\"params\""));
    }

    #[test]
    fn test_parse_response_with_result() {
        let text = r#"{"jsonrpc":"2.0","id":42,"result":{"key":"value"}}"#;
        let msg = parse_incoming(text).unwrap();
        match msg {
            Incoming::Response { id, result } => {
                assert_eq!(id, json!(42));
                assert_eq!(result, json!({"key": "value"}));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_parse_error_response() {
        let text =
            r#"{"jsonrpc":"2.0","id":"abc","error":{"code":-32600,"message":"Invalid Request"}}"#;
        let msg = parse_incoming(text).unwrap();
        match msg {
            Incoming::Error { id, error } => {
                assert_eq!(id, json!("abc"));
                assert_eq!(error.code, -32600);
                assert_eq!(error.message, "Invalid Request");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_parse_notification() {
        let text = r#"{"jsonrpc":"2.0","method":"tools/list","params":{"cursor":null}}"#;
        let msg = parse_incoming(text).unwrap();
        match msg {
            Incoming::Notification { method, params } => {
                assert_eq!(method, "tools/list");
                assert_eq!(params, json!({"cursor": null}));
            }
            _ => panic!("expected Notification"),
        }
    }

    #[test]
    fn test_parse_request() {
        let text = r#"{"jsonrpc":"2.0","id":99,"method":"echo","params":{}}"#;
        let msg = parse_incoming(text).unwrap();
        match msg {
            Incoming::Request { id, method, params } => {
                assert_eq!(id, json!(99));
                assert_eq!(method, "echo");
                assert_eq!(params, json!({}));
            }
            _ => panic!("expected Request"),
        }
    }

    #[test]
    fn test_parse_rejects_batch() {
        let text = r#"[{"jsonrpc":"2.0","id":1,"method":"foo","params":{}}]"#;
        let err = parse_incoming(text).unwrap_err();
        assert!(err.contains("batch"));
    }

    #[test]
    fn test_parse_rejects_missing_jsonrpc() {
        let text = r#"{"id":1,"method":"foo"}"#;
        let err = parse_incoming(text).unwrap_err();
        assert!(err.contains("jsonrpc"));
    }

    #[test]
    fn test_parse_rejects_both_result_and_error() {
        let text = r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"err"}}"#;
        let err = parse_incoming(text).unwrap_err();
        assert!(err.contains("both result and error"));
    }

    #[test]
    fn test_modern_meta_merges_into_empty() {
        let result = modern_meta(Value::Null);
        let obj = result.as_object().unwrap();
        let meta = obj.get("_meta").unwrap().as_object().unwrap();
        assert_eq!(
            meta.get("io.modelcontextprotocol/protocolVersion").unwrap(),
            &json!(MODERN_VERSION)
        );
        assert!(meta.contains_key("io.modelcontextprotocol/clientInfo"));
    }

    #[test]
    fn test_modern_meta_merges_into_existing() {
        let input = json!({"_meta": {"existing": "value"}, "other": "data"});
        let result = modern_meta(input);
        let obj = result.as_object().unwrap();
        let meta = obj.get("_meta").unwrap().as_object().unwrap();
        assert_eq!(meta.get("existing").unwrap(), &json!("value"));
        assert_eq!(
            meta.get("io.modelcontextprotocol/protocolVersion").unwrap(),
            &json!(MODERN_VERSION)
        );
        assert_eq!(obj.get("other").unwrap(), &json!("data"));
    }

    #[test]
    fn test_initialize_params() {
        let params = initialize_params();
        let obj = params.as_object().unwrap();
        assert_eq!(obj.get("protocolVersion").unwrap(), &json!(LEGACY_VERSION));
        assert!(obj.contains_key("capabilities"));
        let client_info = obj.get("clientInfo").unwrap().as_object().unwrap();
        assert_eq!(client_info.get("name").unwrap(), &json!("otto"));
    }

    #[test]
    fn test_decode_initialize_success() {
        let result = json!({"protocolVersion": "2025-11-25"});
        let version = decode_initialize(&result).unwrap();
        assert_eq!(version, "2025-11-25");
    }

    #[test]
    fn test_decode_initialize_missing_version() {
        let result = json!({});
        let err = decode_initialize(&result).unwrap_err();
        assert!(err.contains("protocolVersion"));
    }

    #[test]
    fn test_decode_initialize_empty_version() {
        let result = json!({"protocolVersion": ""});
        let err = decode_initialize(&result).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn test_decode_tools_list_success() {
        let result = json!({
            "tools": [
                {"name": "tool1", "description": "desc1"},
                {"name": "tool2"}
            ],
            "nextCursor": "abc123"
        });
        let (tools, cursor) = decode_tools_list(&result).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool1");
        assert_eq!(cursor, Some("abc123".to_string()));
    }

    #[test]
    fn test_decode_tools_list_skips_undecodable() {
        let result = json!({
            "tools": [
                {"name": "tool1"},
                {"invalid": "entry"},
                {"name": "tool2"}
            ]
        });
        let (tools, _) = decode_tools_list(&result).unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn test_decode_tools_list_empty_cursor_ignored() {
        let result = json!({
            "tools": [{"name": "t"}],
            "nextCursor": ""
        });
        let (_, cursor) = decode_tools_list(&result).unwrap();
        assert_eq!(cursor, None);
    }

    #[test]
    fn test_decode_call_result_text_content() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello"}
            ],
            "isError": false
        });
        let outcome = decode_call_result(&result, &Era::Legacy("2025-11-25".to_string())).unwrap();
        assert_eq!(outcome.content.len(), 1);
        assert!(matches!(outcome.content[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn test_decode_call_result_image_content() {
        let result = json!({
            "content": [
                {"type": "image", "data": "dGVzdA==", "mimeType": "image/png"}
            ]
        });
        let outcome = decode_call_result(&result, &Era::Legacy("2025-11-25".to_string())).unwrap();
        assert_eq!(outcome.content.len(), 1);
        match &outcome.content[0] {
            ContentBlock::Image { data_len, .. } => assert_eq!(*data_len, 8),
            _ => panic!("expected Image"),
        }
    }

    #[test]
    fn test_decode_call_result_resource_link() {
        let result = json!({
            "content": [
                {"type": "resource_link", "uri": "file:///test", "name": "file.txt", "title": "My File"}
            ]
        });
        let outcome = decode_call_result(&result, &Era::Legacy("2025-11-25".to_string())).unwrap();
        match &outcome.content[0] {
            ContentBlock::ResourceLink { uri, title, .. } => {
                assert_eq!(uri, "file:///test");
                assert_eq!(title, &Some("My File".to_string()));
            }
            _ => panic!("expected ResourceLink"),
        }
    }

    #[test]
    fn test_decode_call_result_resource_with_blob() {
        let result = json!({
            "content": [
                {"type": "resource", "resource": {"uri": "data:", "blob": "dGVzdGRhdGE="}}
            ]
        });
        let outcome = decode_call_result(&result, &Era::Legacy("2025-11-25".to_string())).unwrap();
        match &outcome.content[0] {
            ContentBlock::Resource { blob_len, .. } => assert_eq!(*blob_len, 12),
            _ => panic!("expected Resource"),
        }
    }

    #[test]
    fn test_decode_call_result_unknown_content_type_skipped() {
        let result = json!({
            "content": [
                {"type": "unknown_type", "data": "ignored"},
                {"type": "text", "text": "real"}
            ]
        });
        let outcome = decode_call_result(&result, &Era::Legacy("2025-11-25".to_string())).unwrap();
        assert_eq!(outcome.content.len(), 1);
    }

    #[test]
    fn test_decode_call_result_modern_unsupported_result_type() {
        let result = json!({
            "resultType": "progress",
            "content": []
        });
        let err = decode_call_result(&result, &Era::Modern).unwrap_err();
        assert!(err.contains("unsupported resultType"));
    }

    #[test]
    fn test_decode_call_result_modern_complete_result_type() {
        let result = json!({
            "resultType": "complete",
            "content": []
        });
        let outcome = decode_call_result(&result, &Era::Modern).unwrap();
        assert_eq!(outcome.content.len(), 0);
    }

    #[test]
    fn test_decode_call_result_structured_content() {
        let result = json!({
            "content": [],
            "structuredContent": {"nested": {"data": true}}
        });
        let outcome = decode_call_result(&result, &Era::Legacy("2025-11-25".to_string())).unwrap();
        assert_eq!(
            outcome.structured_content,
            Some(json!({"nested": {"data": true}}))
        );
    }

    #[test]
    fn test_supports_modern_true() {
        let error = RpcError {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "not supported".to_string(),
            data: Some(json!({"supported": [MODERN_VERSION, "2025-11-25"]})),
        };
        assert!(supports_modern(&error));
    }

    #[test]
    fn test_supports_modern_false_wrong_code() {
        let error = RpcError {
            code: -32000,
            message: "other error".to_string(),
            data: Some(json!({"supported": [MODERN_VERSION]})),
        };
        assert!(!supports_modern(&error));
    }

    #[test]
    fn test_supports_modern_false_no_modern_in_list() {
        let error = RpcError {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "not supported".to_string(),
            data: Some(json!({"supported": ["2025-11-25"]})),
        };
        assert!(!supports_modern(&error));
    }
}
