//! WebAssembly bindings over `otto-core` for the browser UI.
//!
//! The exports are the SSE frame reader, the transcript reducer, and the
//! TypeScript declarations for the `otto serve` wire types. `ui/src` imports
//! all three from the generated package instead of reimplementing them, so
//! there is one definition of the wire format for the server and the browser.
//!
//! `unsafe_code` is allowed here because `wasm_bindgen` generates the unsafe
//! glue for every export. No hand-written unsafe code belongs in this crate.
#![allow(unsafe_code)]

use otto_core::model::Message;
use otto_core::wire::sse;
use otto_core::wire::transcript::{self, Item};
use wasm_bindgen::prelude::*;

/// Decodes one transcript message from JSON and applies
/// [`Message::validate`].
///
/// Returns `Ok(())` when the message is valid. The rejection is a JS string:
/// either the serde decode error or the validation message, which is the same
/// text the native binary reports.
#[wasm_bindgen]
pub fn validate_message_json(json: &str) -> Result<(), JsValue> {
    let message: Message =
        serde_json::from_str(json).map_err(|error| JsValue::from_str(&error.to_string()))?;
    message
        .validate()
        .map_err(|error| JsValue::from_str(error.0))
}

/// Named TypeScript return types for the exports below. `typescript_type`
/// only renames the signature; the declarations themselves come from
/// `WIRE_TYPES`.
#[wasm_bindgen]
unsafe extern "C" {
    #[wasm_bindgen(typescript_type = "ParsedFrames")]
    pub type ParsedFramesJs;
    #[wasm_bindgen(typescript_type = "Item[]")]
    pub type ItemArray;
}

/// The wire and transcript types the UI imports. wasm-bindgen copies this
/// verbatim into the generated `.d.ts`, which replaces `ui/src/types.ts`.
#[wasm_bindgen(typescript_custom_section)]
const WIRE_TYPES: &'static str = r#"
export interface Usage {
  input_tokens: number
  output_tokens: number
  cached_input_tokens?: number
}

export type TurnStatus = 'running' | 'ok' | 'error' | 'canceled'

export interface SessionTurn {
  id: string
  trigger: 'user' | 'task'
  status: TurnStatus
}

export interface Sandbox {
  mode: string
  network: string
  bash_available: boolean
  summary: string
}

export interface Session {
  id: string
  name?: string
  workspace: string
  provider: string
  profile: string
  model: string
  thinking: string
  context_window: number
  usage: Usage
  context_input_tokens: number
  sandbox: Sandbox
  turn: SessionTurn | null
}

export interface SessionListRow {
  id: string
  name?: string
  path?: string
  workspace?: string
  provider?: string
  model?: string
  open: boolean
}

export interface TurnSummary {
  id: string
  trigger: 'user' | 'task'
  status: TurnStatus
  error?: string
  text: string
  usage: Usage
  usage_present: boolean
  started_at: string
  finished_at?: string
}

export interface Block {
  type: 'text' | 'image' | 'tool_call' | 'tool_result'
  text?: string
  data?: string
  mime_type?: string
  tool_call_id?: string
  tool_name?: string
  arguments?: unknown
  is_error?: boolean
}

export interface Message {
  id: string
  role: 'user' | 'assistant' | 'tool' | 'context'
  blocks: Block[]
  created_at: string
  display?: boolean
  context_type?: string
}

export interface Compaction {
  checkpoint_id?: string
  reason: string
  tokens_before: number
  estimated_tokens_after: number
  automatic: boolean
  usage?: Usage
  noop: boolean
}

export interface WireEvent {
  type:
    | 'agent_started'
    | 'agent_finished'
    | 'text_delta'
    | 'tool_call_started'
    | 'tool_call_finished'
    | 'provider_usage'
    | 'provider_api_call'
    | 'compaction_started'
    | 'compaction_planned'
    | 'compaction_completed'
    | 'compaction_warning'
    | 'memory_warning'
    | 'agent_error'
    | 'notification'
  turn_id?: string
  task_id?: string
  text?: string
  tool_name?: string
  tool_call_id?: string
  tool_args?: unknown
  result?: { content: string; is_error: boolean }
  usage?: Usage
  usage_present?: boolean
  compaction?: Compaction
  error?: string
}

export interface Task {
  id: string
  name?: string
  agent: string
  description: string
  model?: string
  status: 'queued' | 'running' | 'succeeded' | 'failed' | 'canceled'
  created_at: string
  started_at?: string
  finished_at?: string
  steps: number
  tool_calls: number
  last_tool?: string
  last_text?: string
  usage: Usage
  usage_present: boolean
  result?: string
  error?: string
}

export interface TaskDetail extends Task {
  history: Message[]
}

export interface Info {
  workspace: string
  provider: string
  profile: string
  model: string
  thinking: string
  sandbox: string
  profiles: string[]
}

export interface Frame {
  id: number | null
  event: string
  data: string
}

export interface ParsedFrames {
  frames: Frame[]
  rest: string
}

export type Item =
  | { kind: 'user'; text: string; created_at?: string }
  | { kind: 'image'; data: string; mime_type: string; created_at?: string }
  | { kind: 'assistant'; text: string; created_at?: string }
  | { kind: 'tool'; id: string; name: string; args: string; result?: string; isError?: boolean; created_at?: string }
  | { kind: 'notice'; text: string }
  | { kind: 'error'; text: string }
"#;

/// `None` has to reach JS as `null`, not `undefined`, because `Frame.id` is
/// declared `number | null`.
fn serializer() -> serde_wasm_bindgen::Serializer {
    serde_wasm_bindgen::Serializer::new().serialize_missing_as_null(true)
}

fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    value
        .serialize(&serializer())
        .map_err(|error| JsValue::from_str(&error.to_string()))
}

/// Splits an SSE buffer into whole frames and the unparsed remainder.
///
/// A chunk boundary inside a frame is safe: the partial frame comes back in
/// `rest`.
#[wasm_bindgen(js_name = parseFrames)]
pub fn parse_frames(buf: &str) -> Result<ParsedFramesJs, JsValue> {
    Ok(to_js(&sse::parse_frames(buf))?.unchecked_into())
}

/// Renders stored session history.
///
/// `messages_json` is the JSON body of `GET /v1/sessions/{id}/history`, not a
/// decoded array: tool arguments keep their original key order that way.
#[wasm_bindgen(js_name = fromHistory)]
pub fn from_history(messages_json: &str) -> Result<ItemArray, JsValue> {
    let items = transcript::from_history(messages_json)
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    Ok(to_js(&items)?.unchecked_into())
}

/// Applies one turn event and returns the next transcript.
///
/// `event_json` is the SSE frame's `data` field. `items` is not modified.
#[wasm_bindgen(js_name = reduce)]
pub fn reduce(items: ItemArray, event_json: &str) -> Result<ItemArray, JsValue> {
    let current: Vec<Item> = serde_wasm_bindgen::from_value(items.into())
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    let next = transcript::reduce_json(&current, event_json)
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    Ok(to_js(&next)?.unchecked_into())
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn validate_message_json_accepts_a_well_formed_message() {
        let json = r#"{"id":"m1","role":"user","blocks":[{"type":"text","text":"hi"}],"created_at":"1970-01-01T00:00:10Z"}"#;
        assert!(validate_message_json(json).is_ok());
    }

    #[wasm_bindgen_test]
    fn validate_message_json_reports_the_validation_message() {
        let json = r#"{"id":"m1","role":"user","blocks":[],"created_at":"1970-01-01T00:00:10Z"}"#;
        let error = validate_message_json(json).expect_err("empty user message accepted");
        assert_eq!(
            error.as_string().as_deref(),
            Some("user message content is required")
        );
    }

    #[wasm_bindgen_test]
    fn validate_message_json_reports_a_decode_failure() {
        assert!(validate_message_json("{").is_err());
    }

    #[wasm_bindgen_test]
    fn parse_frames_splits_complete_frames_and_keeps_a_partial_one() {
        let parsed =
            parse_frames("id: 0\nevent: text_delta\ndata: {\"a\":1}\n\nid: 1\nevent: agent_fin")
                .expect("frames parse");
        let value: JsValue = parsed.into();
        let decoded: otto_core::wire::sse::ParsedFrames =
            serde_wasm_bindgen::from_value(value).expect("round trip");
        assert_eq!(decoded.frames.len(), 1);
        assert_eq!(decoded.frames[0].id, Some(0));
        assert_eq!(decoded.rest, "id: 1\nevent: agent_fin");
    }

    #[wasm_bindgen_test]
    fn reduce_merges_text_deltas_across_the_boundary() {
        let empty: ItemArray = to_js(&Vec::<Item>::new())
            .expect("empty items serialize")
            .unchecked_into();
        let once = reduce(empty, r#"{"type":"text_delta","text":"hel"}"#).expect("first delta");
        let twice = reduce(once, r#"{"type":"text_delta","text":"lo"}"#).expect("second delta");
        let items: Vec<Item> = serde_wasm_bindgen::from_value(twice.into()).expect("round trip");
        assert_eq!(
            items,
            vec![Item::Assistant {
                text: "hello".into(),
                created_at: String::new(),
            }]
        );
    }

    #[wasm_bindgen_test]
    fn from_history_pairs_tool_results() {
        let history = r#"[{"id":"1","role":"assistant","blocks":[{"type":"tool_call","tool_call_id":"c1","tool_name":"bash","arguments":{"command":"ls"}}]},
          {"id":"2","role":"tool","blocks":[{"type":"tool_result","tool_call_id":"c1","text":"a.go"}]}]"#;
        let items: Vec<Item> =
            serde_wasm_bindgen::from_value(from_history(history).expect("history").into())
                .expect("round trip");
        assert_eq!(
            items,
            vec![Item::Tool {
                id: "c1".into(),
                name: "bash".into(),
                args: "{\n  \"command\": \"ls\"\n}".into(),
                result: Some("a.go".into()),
                is_error: Some(false),
                created_at: String::new(),
            }]
        );
    }

    #[wasm_bindgen_test]
    fn reduce_reports_a_malformed_event() {
        let empty: ItemArray = to_js(&Vec::<Item>::new())
            .expect("empty items serialize")
            .unchecked_into();
        assert!(reduce(empty, "{").is_err());
    }
}
