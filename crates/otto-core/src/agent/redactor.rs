//! Removes resolved secret values at the provider-neutral agent boundary.
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

/// Provider-bound request fields that may contain prose are transformed on a
/// clone. Structural and provider-opaque fields are checked but never changed.
const MINIMUM_CREDENTIAL_BYTES: usize = 8;
const MINIMUM_PREFIXED_TOKEN_BYTES: usize = 20;
const MAXIMUM_EGRESS_JSON_DEPTH: usize = 128;

fn blocked() -> AgentError {
    AgentError::ProviderEgressSecret
}

fn placeholder(value: &str) -> bool {
    let normalized: String = value
        .trim_matches(|character: char| {
            character.is_ascii_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '`' | '<' | '>' | '[' | ']' | '{' | '}'
                )
        })
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    normalized.is_empty()
        || matches!(
            normalized.as_str(),
            "redacted"
                | "placeholder"
                | "example"
                | "sample"
                | "changeme"
                | "replace"
                | "replaceme"
                | "yourkey"
                | "yourtoken"
                | "yoursecret"
                | "yourpassword"
                | "secret"
                | "password"
                | "token"
                | "apikey"
                | "none"
                | "null"
                | "undefined"
        )
        || normalized.starts_with("example")
        || normalized.starts_with("dummy")
        || normalized.starts_with("testonly")
}

fn sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect();
    matches!(
        normalized.as_str(),
        "TOKEN"
            | "ACCESSTOKEN"
            | "AUTHTOKEN"
            | "BEARERTOKEN"
            | "SECRET"
            | "CLIENTSECRET"
            | "PASSWORD"
            | "PASSWD"
            | "PRIVATEKEY"
            | "SECRETACCESSKEY"
    ) || normalized.ends_with("APIKEY")
        || normalized.ends_with("TOKEN")
        || normalized.ends_with("SECRET")
        || normalized.ends_with("PASSWORD")
        || normalized.ends_with("PRIVATEKEY")
        || normalized.ends_with("SECRETACCESSKEY")
        || normalized.ends_with("ACCESSTOKEN")
}

fn credential_value(value: &str) -> bool {
    let trimmed = value.trim_matches(|character: char| {
        character.is_ascii_whitespace() || matches!(character, '"' | '\'' | '`' | ',' | ';')
    });
    trimmed.len() >= MINIMUM_CREDENTIAL_BYTES && !placeholder(trimmed)
}

fn strong_prefixed_token(value: &str) -> bool {
    let token = value.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_')
    });
    if token.len() < MINIMUM_PREFIXED_TOKEN_BYTES || placeholder(token) {
        return false;
    }
    let strong_prefix = token.starts_with("sk-")
        || token.starts_with("ghp_")
        || token.starts_with("gho_")
        || token.starts_with("github_pat_")
        || token.starts_with("xoxb-")
        || token.starts_with("xoxp-")
        || token.starts_with("xoxa-")
        || token.starts_with("AKIA");
    strong_prefix
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn delimited_value_range(value: &str) -> Option<std::ops::Range<usize>> {
    let leading = value.len() - value.trim_start().len();
    let value = value.get(leading..)?;
    if let Some(quote) = value
        .chars()
        .next()
        .filter(|character| matches!(character, '"' | '\'' | '`'))
    {
        let start = leading + quote.len_utf8();
        let quoted = value.get(quote.len_utf8()..)?;
        let length = quoted.find(quote).unwrap_or(quoted.len());
        return Some(start..start + length);
    }
    let length = value
        .find(|character: char| character.is_ascii_whitespace() || matches!(character, ',' | ';'))
        .unwrap_or(value.len());
    Some(leading..leading + length)
}

fn assignment_finding(line: &str) -> Option<std::ops::Range<usize>> {
    let separator = line.find(['=', ':'])?;
    let key = line.get(..separator)?.trim_matches(|character: char| {
        character.is_ascii_whitespace()
            || (!character.is_ascii_alphanumeric() && !matches!(character, '_' | '-'))
    });
    if !sensitive_key(key) && !key.eq_ignore_ascii_case("authorization") {
        return None;
    }
    let after_separator = separator + 1;
    let mut value = line.get(after_separator..)?;
    let mut offset = after_separator;
    let whitespace = value.len() - value.trim_start().len();
    value = value.get(whitespace..)?;
    offset += whitespace;
    if key.eq_ignore_ascii_case("authorization") {
        let scheme_length = "bearer".len();
        let (Some(scheme), Some(after_scheme)) =
            (value.get(..scheme_length), value.get(scheme_length..))
        else {
            return None;
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let separators = after_scheme
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .map(char::len_utf8)
            .sum::<usize>();
        if separators == 0 {
            return None;
        }
        offset += scheme_length + separators;
        value = after_scheme.get(separators..)?;
    }
    let range = delimited_value_range(value)?;
    let finding = offset + range.start..offset + range.end;
    credential_value(line.get(finding.clone())?).then_some(finding)
}

fn independent_finding_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut findings = Vec::new();
    for label in ["PRIVATE", "RSA PRIVATE", "EC PRIVATE", "OPENSSH PRIVATE"] {
        // Construct PEM boundaries from fragments so repository secret scanners
        // do not mistake the detector itself for embedded key material.
        let begin = format!("-----{} {label} KEY-----", "BEGIN");
        let end = format!("-----{} {label} KEY-----", "END");
        let mut offset = 0;
        while let Some(relative_start) = text[offset..].find(&begin) {
            let start = offset + relative_start;
            let after_begin = start + begin.len();
            let finish = text[after_begin..]
                .find(&end)
                .map_or(text.len(), |relative_end| {
                    after_begin + relative_end + end.len()
                });
            findings.push(start..finish);
            offset = finish;
            if offset == text.len() {
                break;
            }
        }
    }
    let mut offset = 0;
    for token in text.split_ascii_whitespace() {
        let relative = text[offset..].find(token).unwrap_or_default();
        let token_start = offset + relative;
        let trimmed_start = token
            .find(|character: char| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
            .unwrap_or(token.len());
        let trimmed_end = token
            .rfind(|character: char| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
            .map_or(trimmed_start, |index| index + 1);
        if strong_prefixed_token(token) {
            findings.push(token_start + trimmed_start..token_start + trimmed_end);
        }
        offset = token_start + token.len();
    }
    findings
}

fn heuristic_finding_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut findings = independent_finding_ranges(text);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if let Some(range) = assignment_finding(content) {
            findings.push(offset + range.start..offset + range.end);
        }
        offset += line.len();
    }
    findings
}

fn heuristic_secret(text: &str) -> bool {
    !heuristic_finding_ranges(text).is_empty()
}

fn redact_ranges(
    redactor: &Redactor,
    text: &str,
    mut ranges: Vec<std::ops::Range<usize>>,
) -> Option<String> {
    let mut marker_inputs = redactor.values.clone();
    marker_inputs.extend(ranges.iter().map(|range| text[range.clone()].to_owned()));
    let marker = safetext::shared_redaction_marker(&marker_inputs)?;
    ranges.sort_by_key(|range| (range.start, std::cmp::Reverse(range.end)));
    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in ranges {
        if range.start < cursor || range.start >= range.end {
            continue;
        }
        redacted.push_str(&text[cursor..range.start]);
        redacted.push_str(&marker);
        cursor = range.end;
    }
    redacted.push_str(&text[cursor..]);
    Some(redacted)
}

fn json_strings_are_safe(redactor: &Redactor, value: &serde_json::Value, depth: usize) -> bool {
    if depth > MAXIMUM_EGRESS_JSON_DEPTH {
        return false;
    }
    match value {
        serde_json::Value::String(value) => protected_text_is_safe(redactor, value),
        serde_json::Value::Array(values) => values
            .iter()
            .all(|value| json_strings_are_safe(redactor, value, depth + 1)),
        serde_json::Value::Object(values) => values.iter().all(|(key, value)| {
            protected_text_is_safe(redactor, key)
                && !(matches!(value, serde_json::Value::String(value)
                    if sensitive_key(key) && credential_value(value)))
                && json_strings_are_safe(redactor, value, depth + 1)
        }),
        _ => true,
    }
}

fn protected_json_is_safe(redactor: &Redactor, raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    json_strings_are_safe(redactor, &value, 0)
}

fn redact_heuristic_text(redactor: &Redactor, text: &str) -> Option<String> {
    let exact = redactor.redact_string(text);
    let ranges = heuristic_finding_ranges(&exact);
    if ranges.is_empty() {
        return Some(exact);
    }
    redact_ranges(redactor, &exact, ranges)
}

fn protected_text_is_safe(redactor: &Redactor, text: &str) -> bool {
    redactor.redact_string(text) == text && !heuristic_secret(text)
}

fn redact_json_string_value(
    redactor: &Redactor,
    text: &str,
    sensitive_association: bool,
) -> Option<String> {
    let exact = redactor.redact_string(text);
    let mut ranges = independent_finding_ranges(&exact);
    if sensitive_association && credential_value(&exact) {
        ranges.push(0..exact.len());
    }
    if ranges.is_empty() {
        Some(exact)
    } else {
        redact_ranges(redactor, &exact, ranges)
    }
}

fn redact_egress_json(redactor: &Redactor, raw: &str) -> Option<Box<RawValue>> {
    let text = redactor.redact_egress_json_text(raw)?;
    RawValue::from_string(text).ok()
}

fn finish_reason_is_safe(redactor: &Redactor, reason: &crate::model::FinishReason) -> bool {
    match reason {
        crate::model::FinishReason::Other(value) => protected_text_is_safe(redactor, value),
        _ => true,
    }
}

fn role_is_safe(redactor: &Redactor, role: &crate::model::Role) -> bool {
    match role {
        crate::model::Role::Other(value) => protected_text_is_safe(redactor, value),
        _ => true,
    }
}

fn block_type_is_safe(redactor: &Redactor, block_type: &BlockType) -> bool {
    match block_type {
        BlockType::Other(value) => protected_text_is_safe(redactor, value),
        _ => true,
    }
}

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
pub struct Redactor {
    /// Canonical secret forms, longest first then lexicographic, so the
    /// leftmost-longest match wins.
    values: Vec<String>,
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
        Self {
            values: canonical,
            complete: marker.is_some(),
            marker: marker.unwrap_or_default(),
        }
    }

    /// Whether exact boundary redaction has a safe replacement. It exposes no
    /// configured value. When this is false every method fails closed.
    pub fn allows_dynamic_content(&self) -> bool {
        self.complete
    }

    /// Clones a provider request, redacting known and high-confidence secrets
    /// only in free text and JSON string keys/values. Structural and opaque
    /// fields are preserved byte-for-byte or cause a local refusal.
    pub fn guard_provider_request(&self, request: &Request) -> Result<Request, AgentError> {
        if !self.complete
            || !protected_text_is_safe(self, &request.model)
            || !protected_text_is_safe(self, &request.thinking)
            || !protected_text_is_safe(self, &request.cache_key)
        {
            return Err(blocked());
        }
        if let Some(remote) = &request.remote_compaction
            && (!protected_text_is_safe(self, &remote.provider)
                || !protected_text_is_safe(self, &remote.id)
                || !protected_text_is_safe(self, &remote.encrypted_content))
        {
            return Err(blocked());
        }
        for tool in &request.tools {
            if !protected_text_is_safe(self, &tool.name)
                || !protected_text_is_safe(self, &tool.description)
                || tool
                    .parameters
                    .as_ref()
                    .is_some_and(|parameters| !protected_json_is_safe(self, parameters.get()))
            {
                return Err(blocked());
            }
        }

        let mut guarded = request.clone();
        guarded.system_prompt =
            redact_heuristic_text(self, &request.system_prompt).ok_or_else(blocked)?;
        for message in &mut guarded.messages {
            if !protected_text_is_safe(self, &message.id)
                || !role_is_safe(self, &message.role)
                || !protected_text_is_safe(self, &message.context_type)
                || !protected_text_is_safe(self, &message.input_id)
                || message
                    .finish_reason
                    .as_ref()
                    .is_some_and(|reason| !finish_reason_is_safe(self, reason))
                || message
                    .context_metadata
                    .as_ref()
                    .is_some_and(|metadata| !protected_text_is_safe(self, &metadata.task_id))
            {
                return Err(blocked());
            }
            for block in &mut message.blocks {
                if !block_type_is_safe(self, &block.block_type)
                    || !protected_text_is_safe(self, &block.data)
                    || !protected_text_is_safe(self, &block.mime_type)
                    || !protected_text_is_safe(self, &block.tool_call_id)
                    || !protected_text_is_safe(self, &block.tool_name)
                    || block.operation_metadata.as_ref().is_some_and(|metadata| {
                        metadata.operation_id.as_ref().is_some_and(|operation_id| {
                            !protected_text_is_safe(self, operation_id.as_str())
                        })
                    })
                {
                    return Err(blocked());
                }
                block.text = redact_heuristic_text(self, &block.text).ok_or_else(blocked)?;
                if let Some(arguments) = &block.arguments {
                    block.arguments =
                        Some(redact_egress_json(self, arguments.get()).ok_or_else(blocked)?);
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
        let mut redacted = text.to_owned();
        for value in &self.values {
            redacted = redacted.replace(value.as_str(), &self.marker);
        }
        redacted
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

    /// Parses `raw` and re-encodes it with every string redacted. `None` means
    /// fail closed.
    fn redact_json_text(&self, raw: &str) -> Option<String> {
        let mut parser = Parser {
            bytes: raw.as_bytes(),
            index: 0,
        };
        let value = self.parse_value(&mut parser)?;
        parser.skip_whitespace();
        if parser.index != parser.bytes.len() {
            // A second value is not a document.
            return None;
        }
        Some(value)
    }

    /// Parses provider-bound JSON while preserving scalar literals verbatim
    /// and applying heuristic redaction at each string's own tree position.
    fn redact_egress_json_text(&self, raw: &str) -> Option<String> {
        let mut parser = Parser {
            bytes: raw.as_bytes(),
            index: 0,
        };
        let value = self.parse_egress_value(&mut parser)?;
        parser.skip_whitespace();
        (parser.index == parser.bytes.len()).then_some(value)
    }

    fn parse_egress_value(&self, parser: &mut Parser<'_>) -> Option<String> {
        let mut stack: Vec<EgressFrame> = Vec::new();
        'read: loop {
            parser.skip_whitespace();
            let sensitive_association =
                matches!(stack.last(), Some(EgressFrame::Object(_, Some((_, true)))));
            let mut value = match parser.peek()? {
                b'{' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_EGRESS_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b'}' {
                        parser.index += 1;
                        "{}".to_owned()
                    } else {
                        let key = self.parse_egress_member_key(parser)?;
                        stack.push(EgressFrame::Object(Vec::new(), Some(key)));
                        continue 'read;
                    }
                }
                b'[' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_EGRESS_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b']' {
                        parser.index += 1;
                        "[]".to_owned()
                    } else {
                        stack.push(EgressFrame::Array(Vec::new()));
                        continue 'read;
                    }
                }
                b'"' => encode_json_string(&redact_json_string_value(
                    self,
                    &parser.parse_string()?,
                    sensitive_association,
                )?),
                _ => parser.parse_scalar()?,
            };

            loop {
                match stack.last_mut() {
                    None => return Some(value),
                    Some(EgressFrame::Array(items)) => {
                        items.push(value);
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => continue 'read,
                            b']' => {}
                            _ => return None,
                        }
                        let Some(EgressFrame::Array(items)) = stack.pop() else {
                            unreachable!("the frame was just matched as an array")
                        };
                        value = encode_array(&items);
                    }
                    Some(EgressFrame::Object(members, pending_key)) => {
                        let (key, _) = pending_key.take()?;
                        members.push((key, value));
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => {
                                *pending_key = Some(self.parse_egress_member_key(parser)?);
                                continue 'read;
                            }
                            b'}' => {}
                            _ => return None,
                        }
                        let Some(EgressFrame::Object(members, _)) = stack.pop() else {
                            unreachable!("the frame was just matched as an object")
                        };
                        value = encode_object(members);
                    }
                }
            }
        }
    }

    fn parse_egress_member_key(&self, parser: &mut Parser<'_>) -> Option<(String, bool)> {
        parser.skip_whitespace();
        if parser.peek()? != b'"' {
            return None;
        }
        let original = parser.parse_string()?;
        let sensitive = sensitive_key(&original);
        let exact = self.redact_string(&original);
        let ranges = independent_finding_ranges(&exact);
        let key = if ranges.is_empty() {
            exact
        } else {
            redact_ranges(self, &exact, ranges)?
        };
        parser.skip_whitespace();
        if parser.next()? != b':' {
            return None;
        }
        Some((key, sensitive))
    }

    /// An iterative JSON parse that emits redacted JSON text directly. It uses
    /// an explicit stack rather than recursion so the supported depth of
    /// [`MAXIMUM_JSON_DEPTH`] cannot overflow the thread stack.
    fn parse_value(&self, parser: &mut Parser<'_>) -> Option<String> {
        let mut stack: Vec<Frame> = Vec::new();
        'read: loop {
            parser.skip_whitespace();
            let mut value = match parser.peek()? {
                b'{' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b'}' {
                        parser.index += 1;
                        "{}".to_owned()
                    } else {
                        let key = self.parse_member_key(parser)?;
                        stack.push(Frame::Object(Vec::new(), Some(key)));
                        continue 'read;
                    }
                }
                b'[' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b']' {
                        parser.index += 1;
                        "[]".to_owned()
                    } else {
                        stack.push(Frame::Array(Vec::new()));
                        continue 'read;
                    }
                }
                b'"' => encode_json_string(&self.redact_string(&parser.parse_string()?)),
                _ => parser.parse_scalar()?,
            };

            // Hand the finished value to its container, closing containers
            // that have no members left.
            loop {
                match stack.last_mut() {
                    None => return Some(value),
                    Some(Frame::Array(items)) => {
                        items.push(value);
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => continue 'read,
                            b']' => {}
                            _ => return None,
                        }
                        let Some(Frame::Array(items)) = stack.pop() else {
                            unreachable!("the frame was just matched as an array")
                        };
                        value = encode_array(&items);
                    }
                    Some(Frame::Object(members, pending_key)) => {
                        members.push((pending_key.take()?, value));
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => {
                                *pending_key = Some(self.parse_member_key(parser)?);
                                continue 'read;
                            }
                            b'}' => {}
                            _ => return None,
                        }
                        let Some(Frame::Object(members, _)) = stack.pop() else {
                            unreachable!("the frame was just matched as an object")
                        };
                        value = encode_object(members);
                    }
                }
            }
        }
    }

    /// A redacted object member name plus its `:` separator.
    fn parse_member_key(&self, parser: &mut Parser<'_>) -> Option<String> {
        parser.skip_whitespace();
        if parser.peek()? != b'"' {
            return None;
        }
        let key = self.redact_string(&parser.parse_string()?);
        parser.skip_whitespace();
        if parser.next()? != b':' {
            return None;
        }
        Some(key)
    }
}

/// A container being filled while [`Redactor::parse_value`] descends.
enum Frame {
    Array(Vec<String>),
    /// Members in source order plus the key whose value is being read.
    Object(Vec<(String, String)>, Option<String>),
}

/// Provider-egress JSON frame retaining the pending key's sensitivity.
enum EgressFrame {
    Array(Vec<String>),
    Object(Vec<(String, String)>, Option<(String, bool)>),
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
        for secret in &self.redactor.values {
            let Some(index) = self.pending.find(secret.as_str()) else {
                continue;
            };
            match first {
                Some((found, _)) if index > found => continue,
                Some((found, length)) if index == found && secret.len() <= length => continue,
                _ => first = Some((index, secret.len())),
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

/// A byte cursor over JSON text.
struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.index += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.index += 1;
        }
    }

    /// Decodes a JSON string starting at the opening quote.
    ///
    /// Escape handling: an unpaired surrogate becomes U+FFFD, and any byte that
    /// is not valid UTF-8 becomes U+FFFD too. A raw control character or an
    /// unknown escape is a parse failure.
    fn parse_string(&mut self) -> Option<String> {
        self.index += 1;
        let mut decoded: Vec<u8> = Vec::new();
        loop {
            match self.next()? {
                b'"' => return Some(safetext::canonicalize_utf8(&decoded)),
                b'\\' => match self.next()? {
                    b'"' => decoded.push(b'"'),
                    b'\\' => decoded.push(b'\\'),
                    b'/' => decoded.push(b'/'),
                    b'b' => decoded.push(0x08),
                    b'f' => decoded.push(0x0c),
                    b'n' => decoded.push(b'\n'),
                    b'r' => decoded.push(b'\r'),
                    b't' => decoded.push(b'\t'),
                    b'u' => {
                        let mut buffer = [0u8; 4];
                        let text = self.parse_unicode_escape()?.encode_utf8(&mut buffer);
                        decoded.extend_from_slice(text.as_bytes());
                    }
                    _ => return None,
                },
                byte if byte < 0x20 => return None,
                byte => decoded.push(byte),
            }
        }
    }

    /// The rune of a `\u` escape, with the leading `\u` already consumed.
    fn parse_unicode_escape(&mut self) -> Option<char> {
        const REPLACEMENT: char = '\u{FFFD}';
        let leading = self.parse_hex4()?;
        if (0xDC00..0xE000).contains(&leading) {
            return Some(REPLACEMENT);
        }
        if !(0xD800..0xDC00).contains(&leading) {
            return char::from_u32(leading).or(Some(REPLACEMENT));
        }
        let resume = self.index;
        if self.bytes.get(self.index) != Some(&b'\\')
            || self.bytes.get(self.index + 1) != Some(&b'u')
        {
            return Some(REPLACEMENT);
        }
        self.index += 2;
        let Some(trailing) = self.parse_hex4() else {
            self.index = resume;
            return Some(REPLACEMENT);
        };
        if !(0xDC00..0xE000).contains(&trailing) {
            self.index = resume;
            return Some(REPLACEMENT);
        }
        let combined = 0x10000 + ((leading - 0xD800) << 10) + (trailing - 0xDC00);
        char::from_u32(combined).or(Some(REPLACEMENT))
    }

    fn parse_hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.index..self.index + 4)?;
        let mut value = 0u32;
        for &digit in digits {
            value = value * 16 + char::from(digit).to_digit(16)?;
        }
        self.index += 4;
        Some(value)
    }

    /// A literal or a number, copied through verbatim so that number
    /// precision and formatting survive.
    fn parse_scalar(&mut self) -> Option<String> {
        for literal in ["true", "false", "null"] {
            if self.bytes[self.index..].starts_with(literal.as_bytes()) {
                self.index += literal.len();
                return Some(literal.to_owned());
            }
        }
        let start = self.index;
        if self.peek()? == b'-' {
            self.index += 1;
        }
        match self.peek()? {
            b'0' => self.index += 1,
            b'1'..=b'9' => self.skip_digits(),
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.index += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            self.skip_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            self.skip_digits();
        }
        String::from_utf8(self.bytes[start..self.index].to_vec()).ok()
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.index += 1;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn values(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn redact_json(credentials: &[&str], raw: &str) -> String {
        Redactor::new(&values(credentials))
            .redact_json_strings(raw)
            .get()
            .to_owned()
    }

    /// Every non-control rune, so marker selection has nothing left to pick.
    fn all_non_control_runes(excluded: &[char]) -> String {
        let mut value = String::new();
        for candidate in 1u32..=char::MAX as u32 {
            let Some(character) = char::from_u32(candidate) else {
                continue;
            };
            if character.is_control() || excluded.contains(&character) {
                continue;
            }
            value.push(character);
        }
        assert!(
            value.contains(REDACTION_MARKER),
            "fixture omitted the marker"
        );
        value
    }

    fn candidate() -> String {
        ["gh", "p_", "abcdefghijkl", "mnopqrstuvwx"].concat()
    }

    #[test]
    fn egress_guard_detects_high_confidence_forms_and_ignores_placeholders() {
        let token = candidate();
        assert!(heuristic_secret(&format!("Authorization: Bearer {token}")));
        assert!(heuristic_secret(&format!(
            "Authorization: BeArEr   \t{token}"
        )));
        assert!(heuristic_secret(&format!("private-key = {token}")));
        assert!(heuristic_secret(
            &[
                "-----BEGIN ",
                "PRIVATE KEY-----\nbody\n-----END PRIVATE KEY-----",
            ]
            .concat()
        ));
        assert!(strong_prefixed_token(&token));
        assert!(!strong_prefixed_token("ghp_example"));
        assert!(!heuristic_secret("API_KEY=<redacted>"));
        assert!(!heuristic_secret("TOKEN=your-token"));
        assert!(!heuristic_secret(
            "The authorization requirements use a bearer identity."
        ));
    }

    #[test]
    fn egress_guard_preserves_non_ascii_authorization_value() {
        let authorization = "Authorization: €€€";
        let request = Request {
            system_prompt: authorization.into(),
            ..Request::default()
        };
        let guarded = Redactor::new(&[])
            .guard_provider_request(&request)
            .expect("non-Bearer authorization is benign");
        assert_eq!(guarded.system_prompt, authorization);
    }

    #[test]
    fn egress_guard_preserves_benign_authorization_and_bearer_prose() {
        let prose = "Discuss authorization requirements and bearer identity semantics.";
        let request = Request {
            system_prompt: prose.into(),
            messages: vec![crate::model::Message {
                role: crate::model::Role::User,
                blocks: vec![crate::model::Block::text(prose)],
                ..crate::model::Message::default()
            }],
            ..Request::default()
        };
        let guarded = Redactor::new(&[])
            .guard_provider_request(&request)
            .expect("benign prose");
        assert_eq!(guarded.system_prompt, prose);
        assert_eq!(guarded.messages[0].blocks[0].text, prose);

        let token = candidate();
        let assignment = Request {
            system_prompt: format!("prefix API_KEY={token} suffix"),
            ..Request::default()
        };
        let guarded = Redactor::new(&[])
            .guard_provider_request(&assignment)
            .expect("sensitive assignment");
        assert!(!guarded.system_prompt.contains(&token));
        assert!(guarded.system_prompt.starts_with("prefix API_KEY="));
        assert!(guarded.system_prompt.ends_with(" suffix"));
    }

    #[test]
    fn egress_guard_redacts_only_the_assigned_occurrence() {
        let candidate = "abcdefgh";
        let request = Request {
            system_prompt: format!("API_KEY={candidate}\nThe unrelated identifier is {candidate}"),
            ..Request::default()
        };
        let guarded = Redactor::new(&[])
            .guard_provider_request(&request)
            .expect("guarded request");
        assert!(
            !guarded
                .system_prompt
                .starts_with(&format!("API_KEY={candidate}"))
        );
        assert!(
            guarded
                .system_prompt
                .ends_with(&format!("identifier is {candidate}"))
        );
    }

    #[test]
    fn egress_guard_redacts_json_strings_and_preserves_valid_json() {
        let token = candidate();
        let request = Request {
            system_prompt: format!("API_KEY={token}"),
            messages: vec![crate::model::Message {
                role: crate::model::Role::Assistant,
                blocks: vec![crate::model::Block {
                    block_type: BlockType::ToolCall,
                    tool_name: "echo".into(),
                    tool_call_id: "call-1".into(),
                    arguments: Some(
                        RawValue::from_string(format!(r#"{{"{token}":{{"api_key":"{token}"}}}}"#))
                            .expect("valid JSON"),
                    ),
                    ..crate::model::Block::default()
                }],
                ..crate::model::Message::default()
            }],
            ..Request::default()
        };
        let guarded = Redactor::new(&[])
            .guard_provider_request(&request)
            .expect("guarded request");
        assert!(!guarded.system_prompt.contains(&token));
        let arguments = guarded.messages[0].blocks[0]
            .arguments
            .as_ref()
            .expect("arguments")
            .get();
        assert!(!arguments.contains(&token));
        assert!(serde_json::from_str::<serde_json::Value>(arguments).is_ok());
        assert!(
            request.system_prompt.contains(&token),
            "input clone unchanged"
        );
    }

    #[test]
    fn egress_guard_associates_json_sensitive_keys_with_their_values() {
        let candidate = "abcdefgh";
        let request = Request {
            messages: vec![crate::model::Message {
                role: crate::model::Role::Assistant,
                blocks: vec![crate::model::Block {
                    block_type: BlockType::ToolCall,
                    arguments: Some(
                        RawValue::from_string(format!(
                            r#"{{"token":"redacted","api_key":"{candidate}","query":"{candidate}"}}"#
                        ))
                        .expect("valid JSON")),
                    ..crate::model::Block::default()
                }],
                ..crate::model::Message::default()
            }],
            ..Request::default()
        };
        let guarded = Redactor::new(&[])
            .guard_provider_request(&request)
            .expect("guarded request");
        let arguments = guarded.messages[0].blocks[0]
            .arguments
            .as_ref()
            .expect("arguments")
            .get();
        let parsed: serde_json::Value = serde_json::from_str(arguments).expect("valid JSON");
        assert_eq!(parsed["token"], "redacted");
        assert_eq!(parsed["query"], candidate);
        assert_ne!(parsed["api_key"], candidate);
    }

    #[test]
    fn egress_guard_removes_known_exact_secret_and_blocks_incomplete_redactor() {
        let secret = "known-exact-provider-secret";
        let request = Request {
            system_prompt: format!("prefix API_KEY={secret} suffix"),
            ..Request::default()
        };
        let guarded = Redactor::new(&[secret.into()])
            .guard_provider_request(&request)
            .expect("known secret is removable from prose");
        assert!(!guarded.system_prompt.contains(secret));
        assert!(guarded.system_prompt.starts_with("prefix API_KEY="));
        assert!(guarded.system_prompt.ends_with(" suffix"));
        let incomplete = Redactor {
            values: Vec::new(),
            marker: String::new(),
            complete: false,
        };
        assert!(matches!(
            incomplete.guard_provider_request(&Request::default()),
            Err(AgentError::ProviderEgressSecret)
        ));
    }

    #[test]
    fn egress_guard_refuses_protected_image_and_tool_definition_fields() {
        let token = candidate();
        let image_request = Request {
            messages: vec![crate::model::Message {
                role: crate::model::Role::User,
                blocks: vec![crate::model::Block {
                    block_type: BlockType::Image,
                    data: token.clone(),
                    mime_type: "image/png".into(),
                    ..crate::model::Block::default()
                }],
                ..crate::model::Message::default()
            }],
            ..Request::default()
        };
        assert!(matches!(
            Redactor::new(&[]).guard_provider_request(&image_request),
            Err(AgentError::ProviderEgressSecret)
        ));
        let tool_request = Request {
            tools: vec![crate::model::ToolDefinition {
                name: "echo".into(),
                description: format!("TOKEN={token}"),
                parameters: None,
            }],
            ..Request::default()
        };
        assert!(matches!(
            Redactor::new(&[]).guard_provider_request(&tool_request),
            Err(AgentError::ProviderEgressSecret)
        ));
        let cache_request = Request {
            cache_key: token.clone(),
            ..Request::default()
        };
        assert!(matches!(
            Redactor::new(&[]).guard_provider_request(&cache_request),
            Err(AgentError::ProviderEgressSecret)
        ));
        let schema_request = Request {
            tools: vec![crate::model::ToolDefinition {
                name: "echo".into(),
                description: "safe".into(),
                parameters: Some(
                    RawValue::from_string(format!(r#"{{"api_key":"{token}"}}"#))
                        .expect("valid schema"),
                ),
            }],
            ..Request::default()
        };
        assert!(matches!(
            Redactor::new(&[]).guard_provider_request(&schema_request),
            Err(AgentError::ProviderEgressSecret)
        ));
    }

    #[test]
    fn expands_json_string_escape_forms_before_marker_selection() {
        let forms = [
            (r"\u003c", "<"),
            (r"\u003C", "<"),
            (r"\u003e", ">"),
            (r"\u0026", "&"),
            (r#"\""#, "\""),
            (r"\\", "\\"),
            (r"\/", "/"),
            (r"\n", "\n"),
            (r"\u2028", "\u{2028}"),
            (r"\u2029", "\u{2029}"),
        ];
        let mut excluded = Vec::new();
        let mut configured = Vec::new();
        for (raw, decoded) in forms {
            excluded.extend(decoded.chars());
            configured.push(raw.to_owned());
        }
        configured.push(all_non_control_runes(&excluded));
        let redactor = Redactor::new(&configured);

        for (raw, decoded) in forms {
            for input in [raw, decoded] {
                let redacted = redactor.redact_string(input);
                assert!(
                    !redacted.contains(raw) && !redacted.contains(decoded),
                    "redact_string({input:?}) retained a configured form: {redacted:?}"
                );
                let encoded = serde_json::to_string(&redacted).expect("encodes");
                assert!(
                    !encoded.contains(raw),
                    "JSON encoding recreated the configured form {raw:?}: {encoded}"
                );
            }
        }
    }

    #[test]
    fn incomplete_redactor_suppresses_every_dynamic_boundary() {
        let redactor = Redactor::with_completeness(&values(&["known-secret"]), false);
        assert_eq!(redactor.redact_string("unknown-attacker-content"), "");
        assert_eq!(
            redactor
                .redact_json_strings(r#"{"unknown":"content"}"#)
                .get(),
            "null"
        );
        let boundary = redactor.redact_error(AgentError::Provider(ProviderError::Cancelled));
        assert!(matches!(
            boundary,
            AgentError::Provider(ProviderError::Cancelled)
        ));
        let mut stream = redactor.new_stream();
        assert_eq!(stream.write("unknown-"), "");
        assert_eq!(stream.write("attacker-content"), "");
        assert_eq!(stream.flush(), "");
    }

    #[test]
    fn incomplete_redactor_replaces_every_other_error_with_an_empty_message() {
        let redactor = Redactor::with_completeness(&values(&["known-secret"]), false);
        let redacted = redactor.redact_error(AgentError::Other("known-secret leaked".into()));
        assert_eq!(redacted.to_string(), "");
        assert!(matches!(redacted, AgentError::Redacted { .. }));
    }

    #[test]
    fn preserves_invalid_compaction_summary_identity() {
        let redactor = Redactor::new(&values(&["secret"]));
        let redacted = redactor.redact_error(AgentError::InvalidCompactionSummary(
            "contains secret".into(),
        ));
        assert!(redacted.is_invalid_compaction_summary());
        assert!(!redacted.to_string().contains("secret"));
    }

    #[test]
    fn preserves_cancellation_identity() {
        let redactor = Redactor::new(&values(&["cancelled"]));
        let redacted = redactor.redact_error(AgentError::Provider(ProviderError::Cancelled));
        assert!(redacted.is_cancelled());
        assert!(!redacted.to_string().contains("cancelled"));
    }

    #[test]
    fn returns_the_original_error_when_nothing_needed_redacting() {
        let redactor = Redactor::new(&values(&["secret"]));
        let redacted = redactor.redact_error(AgentError::EmptyUserText);
        assert!(matches!(redacted, AgentError::EmptyUserText));
    }

    #[test]
    fn uses_stable_single_rune_preferred_markers() {
        let first = Redactor::new(&[]);
        assert_eq!(first.marker, REDACTION_MARKER);
        assert_eq!(first.marker.chars().count(), 1);
        let second = Redactor::new(&values(&[REDACTION_MARKER]));
        assert_eq!(second.marker, "\u{E001}");
        assert_eq!(second.marker.chars().count(), 1);
    }

    #[test]
    fn never_uses_a_replacement_that_contains_the_credential() {
        for credential in ["[REDACTED]", "REDACTED", "[", "界"] {
            let redactor = Redactor::new(&values(&[credential]));
            let redacted = redactor.redact_string(&format!("before {credential} after"));
            assert!(
                !redacted.contains(credential),
                "redact_string() still contains {credential:?}: {redacted:?}"
            );
        }
    }

    #[test]
    fn replacement_cannot_synthesize_another_credential() {
        const SOURCE: &str = "source-secret";
        const SYNTHESIZED: &str = "a[";
        let redactor = Redactor::new(&values(&[SOURCE, SYNTHESIZED]));

        let redacted = redactor.redact_string(&format!("a{SOURCE}"));
        for credential in [SOURCE, SYNTHESIZED] {
            assert!(
                !redacted.contains(credential),
                "redact_string() synthesized {credential:?}: {redacted:?}"
            );
        }

        let mut stream = redactor.new_stream();
        let mut streamed = stream.write("a");
        for character in SOURCE.chars() {
            streamed.push_str(&stream.write(&character.to_string()));
        }
        streamed.push_str(&stream.flush());
        for credential in [SOURCE, SYNTHESIZED] {
            assert!(
                !streamed.contains(credential),
                "stream synthesized {credential:?}: {streamed:?}"
            );
        }
    }

    #[test]
    fn stream_emits_the_same_text_as_a_single_redaction() {
        let redactor = Redactor::new(&values(&["secret", "token-value"]));
        let input = "a token-value and a secret and a sec";
        let mut stream = redactor.new_stream();
        let mut streamed = String::new();
        for character in input.chars() {
            streamed.push_str(&stream.write(&character.to_string()));
        }
        streamed.push_str(&stream.flush());
        assert_eq!(streamed, redactor.redact_string(input));
    }

    #[test]
    fn exhaustive_marker_fallback_suppresses_dynamic_content() {
        let long_prefix = "a".repeat(1 << 20) + "z";
        let redactor = Redactor::new(&[
            all_non_control_runes(&[]),
            long_prefix,
            "X".to_owned(),
            "ab".to_owned(),
        ]);
        assert_eq!(redactor.marker, "");
        assert!(!redactor.allows_dynamic_content());

        let pathological = "a".repeat(50_000) + "X" + &"b".repeat(50_000);
        assert_eq!(redactor.redact_string(&pathological), "");

        let stream_input = "a".repeat(8 << 10) + "X" + &"b".repeat(8 << 10);
        let mut stream = redactor.new_stream();
        for index in 0..stream_input.len() {
            assert_eq!(stream.write(&stream_input[index..index + 1]), "");
        }
        assert_eq!(stream.flush(), "");
    }

    #[test]
    fn redacts_root_and_nested_json_strings() {
        const CREDENTIAL: &str = "json-root-secret";
        for raw in [
            r#""json-root-secret""#,
            r#"{"nested":["json-root-secret"]}"#,
        ] {
            let got = redact_json(&[CREDENTIAL], raw);
            assert!(
                !got.contains(CREDENTIAL),
                "redact_json_strings({raw}) = {got}"
            );
            serde_json::from_str::<serde_json::Value>(&got).expect("valid JSON");
        }
    }

    #[test]
    fn redacts_json_keys_without_losing_number_fidelity() {
        const CREDENTIAL: &str = "resolved-secret";
        let raw = r#"{"resolved-secret":9007199254740993123456789,"nested":{"prefix-resolved-secret-suffix":1.2300e+45}}"#;
        let got = redact_json(&[CREDENTIAL], raw);
        assert!(!got.contains(CREDENTIAL), "leaked the key: {got}");
        for number in ["9007199254740993123456789", "1.2300e+45"] {
            assert!(got.contains(number), "lost {number} fidelity: {got}");
        }
    }

    #[test]
    fn json_key_redaction_is_safe_and_deterministic() {
        let cases: [(&str, Vec<String>, String); 3] = [
            (
                "credential equals default marker",
                values(&[REDACTION_MARKER]),
                format!(r#"{{"{REDACTION_MARKER}":"first","\\uE001":"second"}}"#),
            ),
            (
                "overlapping credentials",
                values(&["secret", "resolved-secret"]),
                r#"{"resolved-secret":{"secret":"value"}}"#.to_owned(),
            ),
            (
                "root and nested collisions",
                values(&["secret"]),
                format!(
                    r#"{{"secret":"first","{REDACTION_MARKER}":"second","nested":{{"secret":1,"{REDACTION_MARKER}":2}}}}"#
                ),
            ),
        ];
        for (name, credentials, raw) in cases {
            let redactor = Redactor::new(&credentials);
            let mut first = String::new();
            for iteration in 0..100 {
                let got = redactor.redact_json_strings(&raw).get().to_owned();
                serde_json::from_str::<serde_json::Value>(&got)
                    .unwrap_or_else(|_| panic!("{name}: invalid JSON {got}"));
                for credential in &credentials {
                    assert!(!got.contains(credential.as_str()), "{name}: leaked {got}");
                }
                if iteration == 0 {
                    first = got;
                } else {
                    assert_eq!(got, first, "{name}: nondeterministic output");
                }
            }
            if name == "root and nested collisions" {
                assert_eq!(
                    first,
                    format!(
                        r#"{{"nested":{{"{REDACTION_MARKER}":null}},"{REDACTION_MARKER}":null}}"#
                    )
                );
            }
        }
    }

    #[test]
    fn json_duplicate_and_normalized_keys_fail_closed() {
        let cases = [
            (
                "exact duplicate",
                r#"{"safe":"first","safe":"attacker"}"#.to_owned(),
                r#"{"safe":null}"#.to_owned(),
            ),
            (
                "escape alias",
                r#"{"a":"first","\u0061":"attacker"}"#.to_owned(),
                r#"{"a":null}"#.to_owned(),
            ),
            (
                "unpaired surrogate variants",
                r#"{"secret-\ud800":"first","secret-\ud801":"attacker"}"#.to_owned(),
                format!(r#"{{"{REDACTION_MARKER}-�":null}}"#),
            ),
            (
                "nested array and post-redaction collision",
                format!(
                    r#"{{"items":[{{"secret":"first","{REDACTION_MARKER}":"attacker"}},{{"nested":{{"a":"first","\u0061":"attacker"}}}}]}}"#
                ),
                format!(r#"{{"items":[{{"{REDACTION_MARKER}":null}},{{"nested":{{"a":null}}}}]}}"#),
            ),
        ];
        for (name, raw, want) in cases {
            let got = redact_json(&["secret"], &raw);
            assert_eq!(got, want, "{name}");
            assert!(
                !got.contains("attacker") && !got.contains("secret"),
                "{name}: retained colliding semantics: {got}"
            );
        }
    }

    #[test]
    fn json_invalid_input_and_depth_fail_closed() {
        let too_deep = "[".repeat(10_001) + r#""secret""# + &"]".repeat(10_001);
        for raw in [
            r#"{"secret":"#.to_owned(),
            r#""unterminated"#.to_owned(),
            r#"{"a":1} {"b":2}"#.to_owned(),
            too_deep,
        ] {
            assert_eq!(redact_json(&["secret"], &raw), "null", "input {raw:.32}");
        }
    }

    #[test]
    fn json_maximum_supported_depth_does_not_panic() {
        let raw = "[".repeat(MAXIMUM_JSON_DEPTH) + r#""secret""# + &"]".repeat(MAXIMUM_JSON_DEPTH);
        let got = redact_json(&["secret"], &raw);
        assert!(!got.contains("secret"));
        assert!(got.starts_with("[["));
    }

    #[test]
    fn overlapping_json_key_secrets_do_not_depend_on_configuration_order() {
        let raw = r#"{"abc":"value"}"#;
        assert_eq!(
            redact_json(&["ab", "bc"], raw),
            redact_json(&["bc", "ab"], raw)
        );
    }

    #[test]
    fn json_without_configured_secrets_passes_through() {
        let redactor = Redactor::new(&[]);
        assert_eq!(
            redactor.redact_json_strings(r#"{"path":"file.go"}"#).get(),
            r#"{"path":"file.go"}"#
        );
    }

    #[test]
    fn escapes_html_characters_the_way_encoding_json_does() {
        assert_eq!(
            redact_json(&["secret"], r#"{"html":"<a>&b"}"#),
            r#"{"html":"\u003ca\u003e\u0026b"}"#
        );
    }
}
