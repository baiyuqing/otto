//! WebAssembly bindings over `otto-core` for the browser UI.
//!
//! Phase 0 exports one function, enough to prove the boundary: the browser can
//! call into the shared core and receive a typed rejection. The stream parser,
//! transcript reducer, and event types follow in phase 6.
//!
//! `unsafe_code` is allowed here because `wasm_bindgen` generates the unsafe
//! glue for every export. No hand-written unsafe code belongs in this crate.
#![allow(unsafe_code)]

use otto_core::model::Message;
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
}
