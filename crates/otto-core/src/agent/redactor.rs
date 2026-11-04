//! Removes resolved secrets and high-confidence PII at the provider-neutral
//! agent boundary.
//!
//! The agent runs every tool result, every streamed text delta, and every error
//! string through a [`Redactor`] before it reaches an event sink or the session
//! file.
//!
//! Ownership: a [`Redactor`] owns its secret values and never exposes them.
//! There is no accessor and no `Debug` output that could print one.
//!
//! Concurrency: a [`Redactor`] is immutable after construction and is `Sync`. A
//! [`StreamRedactor`] holds mutable carry-over state and belongs to one stream.
//!
//! Errors: output redaction fails closed by dropping unsafe text. Provider
//! request guarding returns a payload-free [`AgentError::ProviderEgressSecret`]
//! when a safe transformed clone cannot be produced.

use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::model::BlockType;
use crate::provider::{ProviderError, Request};
use crate::safetext;

use super::events::AgentError;

/// The preferred replacement rune.
pub const REDACTION_MARKER: &str = "\u{E000}";

/// How deeply [`Redactor::redact_json_strings`] will descend before it fails
/// closed.
const MAXIMUM_JSON_DEPTH: usize = 10_000;

/// Replaces known secret values with a marker rune.
///
/// Construct one with [`Redactor::new`] when the secret set is fully known,
/// or [`Redactor::with_completeness`] when the caller could not enumerate
/// every value. An incomplete redactor suppresses all dynamic content,
/// because it cannot promise that what it passes through is safe.
#[derive(Clone)]
pub struct Redactor {
    /// Canonical secret forms, longest first then lexicographic, so the
    /// leftmost-longest match wins.
    values: Vec<String>,
    /// One substring finder per entry of `values`, in the same order.
    finders: Vec<memchr::memmem::Finder<'static>>,
    /// Byte length of the shortest entry of `values`; 0 when there are none.
    shortest_value_bytes: usize,
    /// The replacement text. Empty exactly when `complete` is false.
    marker: String,
    complete: bool,
}

impl Redactor {
    /// A redactor over a fully enumerated secret set.
    pub fn new(values: &[String]) -> Self {
        Self::with_completeness(values, true)
    }

    /// A redactor that knows whether its input was complete. Passing `false`
    /// turns it into a suppressing redactor: it emits nothing rather than
    /// text it cannot vouch for.
    pub fn with_completeness(values: &[String], complete: bool) -> Self {
        if !complete {
            return Self {
                values: Vec::new(),
                finders: Vec::new(),
                shortest_value_bytes: 0,
                marker: String::new(),
                complete: false,
            };
        }
        let mut seen = std::collections::HashSet::new();
        let mut canonical = Vec::new();
        for value in values {
            for form in safetext::secret_forms(value.as_bytes()) {
                if seen.insert(form.clone()) {
                    canonical.push(form);
                }
            }
        }
        safetext::sort_longest_first(&mut canonical);
        let marker = safetext::dynamic_redaction_marker(&canonical);
        let finders = canonical
            .iter()
            .map(|value| memchr::memmem::Finder::new(value.as_bytes()).into_owned())
            .collect();
        let shortest_value_bytes = canonical.iter().map(String::len).min().unwrap_or(0);
        Self {
            values: canonical,
            finders,
            shortest_value_bytes,
            complete: marker.is_some(),
            marker: marker.unwrap_or_default(),
        }
    }

    /// Whether exact boundary redaction has a safe replacement. It exposes no
    /// configured value. When this is false every method fails closed.
    pub fn allows_dynamic_content(&self) -> bool {
        self.complete
    }

    /// Redacts known and high-confidence heuristic secrets from provider-bound
    /// free text, or refuses when the boundary cannot transform it safely.
    pub(super) fn redact_provider_text(&self, text: &str) -> Result<String, AgentError> {
        if !self.complete {
            return Err(AgentError::ProviderEgressSecret);
        }
        redact_heuristic_text(self, text).ok_or(AgentError::ProviderEgressSecret)
    }

    /// Clones a provider request, redacting known/high-confidence secrets and
    /// PII only in free text and JSON values. Structural and opaque fields are
    /// preserved byte-for-byte or cause a local refusal.
    pub fn guard_provider_request(&self, request: &Request) -> Result<Request, AgentError> {
        let mut pii = PiiRedactor::default();
        self.guard_provider_request_internal(request, Some(&mut pii))
    }

    /// Sanitizes transient compaction input before it is assembled into the
    /// complete request. The final provider guard owns request-wide PII names.
    pub(super) fn guard_provider_request_secrets_only(
        &self,
        request: &Request,
    ) -> Result<Request, AgentError> {
        self.guard_provider_request_internal(request, None)
    }

    fn guard_provider_request_internal(
        &self,
        request: &Request,
        mut pii: Option<&mut PiiRedactor>,
    ) -> Result<Request, AgentError> {
        let detect_pii = pii.is_some();
        if !self.complete
            || !protected_text_is_safe(self, &request.model, detect_pii)
            || !protected_text_is_safe(self, &request.thinking, detect_pii)
            || !protected_text_is_safe(self, &request.cache_key, detect_pii)
        {
            return Err(blocked());
        }
        if let Some(remote) = &request.remote_compaction
            && (!protected_text_is_safe(self, &remote.provider, detect_pii)
                || !protected_text_is_safe(self, &remote.id, detect_pii)
                || !protected_text_is_safe(self, &remote.encrypted_content, detect_pii))
        {
            return Err(blocked());
        }
        for tool in &request.tools {
            if !protected_text_is_safe(self, &tool.name, detect_pii)
                || !protected_text_is_safe(self, &tool.description, detect_pii)
                || tool.parameters.as_ref().is_some_and(|parameters| {
                    !protected_json_is_safe(self, parameters.get(), detect_pii)
                })
            {
                return Err(blocked());
            }
        }

        let mut guarded = request.clone();
        guarded.system_prompt =
            redact_provider_text_value(self, pii.as_deref_mut(), &request.system_prompt)
                .ok_or_else(blocked)?;
        for message in &mut guarded.messages {
            if !protected_text_is_safe(self, &message.id, detect_pii)
                || !role_is_safe(self, &message.role, detect_pii)
                || !protected_text_is_safe(self, &message.context_type, detect_pii)
                || !protected_text_is_safe(self, &message.input_id, detect_pii)
                || message
                    .finish_reason
                    .as_ref()
                    .is_some_and(|reason| !finish_reason_is_safe(self, reason, detect_pii))
                || message.context_metadata.as_ref().is_some_and(|metadata| {
                    !protected_text_is_safe(self, &metadata.task_id, detect_pii)
                })
            {
                return Err(blocked());
            }
            for block in &mut message.blocks {
                if !block_type_is_safe(self, &block.block_type, detect_pii)
                    || !protected_text_is_safe(self, &block.data, detect_pii)
                    || !protected_text_is_safe(self, &block.mime_type, detect_pii)
                    || !protected_text_is_safe(self, &block.tool_call_id, detect_pii)
                    || !protected_text_is_safe(self, &block.tool_name, detect_pii)
                    || block.operation_metadata.as_ref().is_some_and(|metadata| {
                        metadata.operation_id.as_ref().is_some_and(|operation_id| {
                            !protected_text_is_safe(self, operation_id.as_str(), detect_pii)
                        })
                    })
                {
                    return Err(blocked());
                }
                block.text = redact_provider_text_value(self, pii.as_deref_mut(), &block.text)
                    .ok_or_else(blocked)?;
                if let Some(arguments) = &block.arguments {
                    block.arguments = Some(
                        redact_egress_json(self, pii.as_deref_mut(), arguments.get())
                            .ok_or_else(blocked)?,
                    );
                }
            }
        }
        Ok(guarded)
    }

    /// The text with every configured secret replaced by the marker, or the
    /// empty string when redaction is not representable.
    pub fn redact_string(&self, text: &str) -> String {
        if text.is_empty() || !self.complete {
            return String::new();
        }
        if self.values.is_empty() {
            return text.to_owned();
        }
        let mut redacted: Option<String> = None;
        for finder in &self.finders {
            let current = redacted.as_deref().unwrap_or(text);
            let mut matches = finder.find_iter(current.as_bytes()).peekable();
            if matches.peek().is_none() {
                continue;
            }
            let mut replaced = String::with_capacity(current.len());
            let mut copied_until = 0;
            for start in matches {
                replaced.push_str(&current[copied_until..start]);
                replaced.push_str(&self.marker);
                copied_until = start + finder.needle().len();
            }
            replaced.push_str(&current[copied_until..]);
            redacted = Some(replaced);
        }
        redacted.unwrap_or_else(|| text.to_owned())
    }

    /// Whether any configured secret value occurs in `text`. False for an
    /// incomplete redactor, which holds no values.
    pub(super) fn contains_secret(&self, text: &str) -> bool {
        if !self.complete || self.values.is_empty() || text.len() < self.shortest_value_bytes {
            return false;
        }
        self.finders
            .iter()
            .any(|finder| finder.find(text.as_bytes()).is_some())
    }

    /// Whether `redact_string(text) == text`, without building the redacted
    /// text. The marker occurs in no value, so sequential replacement changes
    /// the text exactly when some value occurs in the original text.
    pub(super) fn unchanged_by_redaction(&self, text: &str) -> bool {
        text.is_empty() || (self.complete && !self.contains_secret(text))
    }

    /// Redacts every string in a JSON document, keys included, and returns
    /// the re-encoded result.
    ///
    /// `raw` is JSON text; it does not have to be valid. Anything that fails
    /// to parse, nests deeper than [`MAXIMUM_JSON_DEPTH`], or carries trailing
    /// content becomes the literal `null`. Object members whose keys collide
    /// after redaction lose their values to `null` rather than letting the
    /// caller pick an attacker-controlled winner. Number literals are copied
    /// through verbatim, so precision survives.
    pub fn redact_json_strings(&self, raw: &str) -> Box<RawValue> {
        if !self.complete {
            return null_json();
        }
        if self.values.is_empty() {
            return RawValue::from_string(raw.to_owned()).unwrap_or_else(|_| null_json());
        }
        match self.redact_json_text(raw) {
            Some(text) => RawValue::from_string(text).unwrap_or_else(|_| null_json()),
            None => null_json(),
        }
    }

    /// The error with its message redacted.
    ///
    /// The returned error still answers [`AgentError::is_cancelled`] and the
    /// other classifiers the same way the input did, because callers branch on
    /// them. When nothing needed redacting the input is returned unchanged.
    ///
    /// A suppressing redactor passes the payload-free sentinels through by
    /// identity and replaces everything else with an empty message, so an
    /// attacker-supplied string cannot survive.
    pub fn redact_error(&self, error: AgentError) -> AgentError {
        if !self.complete {
            return match &error {
                AgentError::Provider(
                    ProviderError::Cancelled | ProviderError::DeadlineExceeded,
                )
                | AgentError::EmptyUserText
                | AgentError::ProviderEgressSecret => error,
                AgentError::InvalidCompactionSummary(cause) if cause.is_empty() => error,
                _ => AgentError::Redacted {
                    message: String::new(),
                    cancelled: false,
                    empty_user_text: false,
                    invalid_compaction_summary: false,
                },
            };
        }
        let original = error.to_string();
        let message = self.redact_string(&original);
        if message == original {
            return error;
        }
        AgentError::Redacted {
            message,
            cancelled: error.is_cancelled(),
            empty_user_text: error.is_empty_user_text(),
            invalid_compaction_summary: error.is_invalid_compaction_summary(),
        }
    }

    /// A redactor for one text stream. It holds back any suffix that could
    /// still turn into a secret once more text arrives.
    pub fn new_stream(&self) -> StreamRedactor<'_> {
        StreamRedactor {
            redactor: self,
            pending: String::new(),
        }
    }
}

/// Redacts a text stream that arrives in pieces.
///
/// [`StreamRedactor::write`] returns only text that can no longer become part
/// of a secret; the rest is carried until the next write or
/// [`StreamRedactor::flush`]. Borrowing the [`Redactor`] keeps the secret set
/// in one place.
pub struct StreamRedactor<'a> {
    redactor: &'a Redactor,
    pending: String,
}

impl StreamRedactor<'_> {
    /// Adds `text` and returns the part that is safe to emit now.
    pub fn write(&mut self, text: &str) -> String {
        if text.is_empty() || !self.redactor.complete {
            return String::new();
        }
        if self.redactor.values.is_empty() {
            return text.to_owned();
        }
        self.pending.push_str(text);
        let mut output = String::new();
        while !self.pending.is_empty() {
            if let Some((index, length)) = self.first_secret() {
                output.push_str(&self.pending[..index]);
                output.push_str(&self.redactor.marker);
                self.pending.drain(..index + length);
                continue;
            }
            let held = self.partial_secret_suffix_bytes();
            let split = self.pending.len() - held;
            output.push_str(&self.pending[..split]);
            self.pending.drain(..split);
            break;
        }
        output
    }

    /// Ends the stream and returns whatever was held back, redacted.
    pub fn flush(&mut self) -> String {
        if !self.redactor.complete {
            self.pending.clear();
            return String::new();
        }
        if self.redactor.values.is_empty() {
            return String::new();
        }
        let pending = std::mem::take(&mut self.pending);
        self.redactor.redact_string(&pending)
    }

    /// The earliest secret in the pending text as `(byte index, byte length)`,
    /// preferring the longest match at that position.
    fn first_secret(&self) -> Option<(usize, usize)> {
        let mut first: Option<(usize, usize)> = None;
        for finder in &self.redactor.finders {
            let Some(index) = finder.find(self.pending.as_bytes()) else {
                continue;
            };
            let secret_bytes = finder.needle().len();
            match first {
                Some((found, _)) if index > found => continue,
                Some((found, length)) if index == found && secret_bytes <= length => continue,
                _ => first = Some((index, secret_bytes)),
            }
        }
        first
    }

    /// How many trailing bytes could still grow into a secret.
    fn partial_secret_suffix_bytes(&self) -> usize {
        let mut held = 0;
        for secret in &self.redactor.values {
            let maximum = self.pending.len().min(secret.len() - 1);
            let mut size = maximum;
            while size > held {
                // A prefix that splits a rune can never be the suffix of the
                // pending text, which is always valid UTF-8.
                if secret.is_char_boundary(size) && self.pending.ends_with(&secret[..size]) {
                    held = size;
                    break;
                }
                size -= 1;
            }
        }
        held
    }
}

fn null_json() -> Box<RawValue> {
    RawValue::from_string("null".to_owned()).expect("null is valid JSON")
}

/// Encodes a string with the HTML escaping of `<`, `>` and `&` and the escaping
/// of U+2028 and U+2029 that the session format uses. Encodes one string, HTML
/// escaping and U+2028/U+2029 escaping included.
pub(super) fn encode_json_string(value: &str) -> String {
    let encoded = serde_json::to_string(value).expect("a string always encodes");
    if !encoded.contains(['<', '>', '&', '\u{2028}', '\u{2029}']) {
        return encoded;
    }
    encoded
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn encode_array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

/// Encodes object members after redaction: duplicate keys, normalized aliases
/// and post-redaction collisions all resolve to `null`, and the result is key
/// sorted.
fn encode_object(members: Vec<(String, String)>) -> String {
    let mut resolved: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (key, value) in members {
        match resolved.entry(key) {
            Entry::Occupied(mut existing) => {
                existing.insert(None);
            }
            Entry::Vacant(slot) => {
                slot.insert(Some(value));
            }
        }
    }
    let mut encoded = String::from("{");
    for (position, (key, value)) in resolved.iter().enumerate() {
        if position > 0 {
            encoded.push(',');
        }
        encoded.push_str(&encode_json_string(key));
        encoded.push(':');
        encoded.push_str(value.as_deref().unwrap_or("null"));
    }
    encoded.push('}');
    encoded
}

mod connection_strings;
mod credential_patterns;
mod heuristics;
mod json_parse;
mod pii;
mod recording;
mod wallet_secrets;

use heuristics::*;
use pii::*;

#[cfg(test)]
mod egress_scan_tests;
#[cfg(test)]
mod pii_tests;
#[cfg(test)]
mod recording_tests;
#[cfg(test)]
mod tests;
