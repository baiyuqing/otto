//! Readable redaction feedback exists only in a provider-bound tool-result copy.
//! Preserve the internal collision-safe markers and the original live result.
use super::*;

impl Redactor {
    pub(super) fn tool_result_for_provider(
        &self,
        pii: Option<&mut PiiRedactor>,
        text: &str,
    ) -> Option<String> {
        let credential_safe = redact_heuristic_text(self, text)?;
        let credentials_changed = credential_safe != text;
        let output = match pii {
            Some(pii) => pii.redact_text(&credential_safe),
            None => credential_safe.clone(),
        };
        let pii_changed = output != credential_safe;
        let has_marker = crate::tool::redaction::contains_placeholder(&output);
        if !credentials_changed && !pii_changed && !has_marker {
            return Some(output);
        }
        let status = if crate::tool::redaction::placeholder_only(&output) {
            "withheld"
        } else {
            "partial"
        };
        let reason = match (credentials_changed, pii_changed) {
            (true, false) => "credential_detection",
            (false, true) => "personal_data",
            _ => "sensitive_content",
        };
        // Sanitize the fixed notice as well: even a common word may equal a
        // configured secret. Never replace the internal marker with unchecked
        // readable text, or modify the actual tool output/JSON on this path.
        let notice = self.redact_string(&format!(
            "\n<otto_redaction status=\"{status}\" reason=\"{reason}\">Sensitive content was withheld. \
            Redaction placeholders are not literal source text or empty results. Use surrounding \
            non-sensitive context; do not copy placeholders into operations or reconstruct hidden content.</otto_redaction>"
        ));
        Some(output + &notice)
    }
}
