//! Credential protection for recording copies. Never use these copies as
//! live tool arguments. Provider guarding additionally applies PII protection.

use super::*;
use crate::model::Message;

impl Redactor {
    /// Known-value and credential-pattern redaction, without PII rewriting.
    /// Valid JSON text is scanned structurally so escaped secrets are decoded.
    pub fn redact_record_text(&self, text: &str) -> String {
        let trimmed = text.trim_start();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Some(json) = self.redact_egress_json_text(None, text)
        {
            // Keep benign JSON prose byte-for-byte, including whitespace and
            // large numeric literals. Compare with the existing exact-only
            // parser rather than decoding numbers through a generic Value.
            if self.redact_string(text) == text
                && self.redact_json_text(text).as_deref() == Some(json.as_str())
            {
                return text.to_owned();
            }
            return json;
        }
        redact_heuristic_text(self, text).unwrap_or_default()
    }

    /// Redacts JSON string values according to their field names. Keys,
    /// numbers and structural identifiers are not renamed by heuristics.
    pub fn redact_record_json(&self, raw: &str) -> Box<RawValue> {
        self.redact_egress_json_text(None, raw)
            .and_then(|json| RawValue::from_string(json).ok())
            .unwrap_or_else(|| RawValue::from_string("null".into()).expect("JSON null"))
    }

    /// Sanitizes content on a clone while retaining message/call identity.
    /// Approval tool content retains its documented scanning exception.
    pub fn redact_record_message(&self, message: &Message) -> Message {
        let mut recorded = message.clone();
        for block in &mut recorded.blocks {
            if matches!(
                block.tool_name.as_str(),
                "request_approval" | "approval_pending" | "approval_revoke"
            ) {
                continue;
            }
            block.text = self.redact_record_text(&block.text);
            if let Some(arguments) = &block.arguments {
                block.arguments = Some(self.redact_record_json(arguments.get()));
            }
        }
        recorded
    }
}
