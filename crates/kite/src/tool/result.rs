//! Output capping, secret redaction, and strict argument decoding.
//!
//! Tool output is untrusted bytes from the filesystem or a child process, so
//! three rules apply to everything that reaches the model:
//!
//! - output is capped at a byte budget and never split in the middle of a
//!   UTF-8 sequence;
//! - invalid UTF-8 is replaced by `U+FFFD` before anything else looks at it;
//! - configured secret values are replaced by a marker using leftmost-longest
//!   matching that also holds back a partial match at the end of a chunk, so a
//!   secret split across two writes is still redacted.
//!
//! Ownership: every type here owns its buffers and is single-owner; none is
//! `Sync`-shared. Concurrency: callers must not share one collector across
//! threads without external synchronization. Errors: the writers cannot fail,
//! so they have no error channel; decoding returns the `encoding/json` error
//! text the model sees.
//!
//! A configured value is a `String` and therefore already valid UTF-8, so only
//! the byte stream needs normalizing.

use serde::de::DeserializeOwned;

use kite_core::tool::ToolResult;

/// The marker used when no dynamic marker was negotiated.
#[cfg(test)]
const LEGACY_REDACTION_MARKER: &str = "[REDACTED]";

/// Collects bytes up to a limit, keeping only whole UTF-8 sequences.
///
/// Once the limit is reached the collector seals: every later byte is counted
/// as discarded and nothing more is retained, so a caller cannot smuggle bytes
/// past the cap by writing again.
#[derive(Debug)]
pub(crate) struct CappedByteCollector {
    limit: usize,
    buf: Vec<u8>,
    discarded: usize,
    sealed: bool,
}

impl CappedByteCollector {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            buf: Vec::new(),
            discarded: 0,
            sealed: false,
        }
    }

    /// Appends as much of `data` as fits, stopping at the first byte that
    /// would split a UTF-8 sequence.
    pub(crate) fn write(&mut self, data: &[u8]) {
        let original = data.len();
        if self.sealed {
            self.discarded += original;
            return;
        }
        let Some(remaining) = self.limit.checked_sub(self.buf.len()).filter(|it| *it > 0) else {
            self.discarded += original;
            self.sealed = true;
            return;
        };
        let retained = complete_utf8_prefix(data, remaining.min(original));
        self.buf.extend_from_slice(&data[..retained]);
        self.discarded += original - retained;
        if retained < original {
            self.sealed = true;
        }
    }

    /// Appends `data` only if all of it fits and is valid UTF-8. Used for the
    /// redaction marker, which must never appear partially written.
    pub(crate) fn write_atomic(&mut self, data: &[u8]) {
        if self.sealed {
            self.discarded += data.len();
            return;
        }
        if data.len() <= self.limit.saturating_sub(self.buf.len())
            && std::str::from_utf8(data).is_ok()
        {
            self.buf.extend_from_slice(data);
        } else {
            self.discarded += data.len();
            self.sealed = true;
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub(crate) fn discarded(&self) -> usize {
        self.discarded
    }
}

/// Returns the length of the longest prefix of `value[..limit]` that ends on a
/// UTF-8 boundary. An invalid byte stops the scan, so invalid input truncates
/// rather than being copied through.
fn complete_utf8_prefix(value: &[u8], limit: usize) -> usize {
    let limit = limit.min(value.len());
    let mut index = 0;
    while index < limit {
        let (invalid, size) = decode_rune(&value[index..limit]);
        if invalid {
            break;
        }
        index += size;
    }
    index
}

/// Decodes the first rune of `data` the way Go's `utf8.DecodeRune` does:
/// an invalid or incomplete sequence reports one byte and an error.
fn decode_rune(data: &[u8]) -> (bool, usize) {
    match std::str::from_utf8(data) {
        Ok(text) => match text.chars().next() {
            Some(character) => (false, character.len_utf8()),
            None => (true, 1),
        },
        Err(error) if error.valid_up_to() > 0 => {
            let head = std::str::from_utf8(&data[..error.valid_up_to()]).unwrap_or_default();
            match head.chars().next() {
                Some(character) => (false, character.len_utf8()),
                None => (true, 1),
            }
        }
        Err(_) => (true, 1),
    }
}

/// Reports whether `data` begins with a complete rune or with an encoding
/// error, matching Go's `utf8.FullRune`.
fn full_rune(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    match std::str::from_utf8(data) {
        Ok(_) => true,
        Err(error) => error.valid_up_to() > 0 || error.error_len().is_some(),
    }
}

/// Returns the longest valid-UTF-8 prefix of `data`, matching
/// `validUTF8Prefix`.
pub(crate) fn valid_utf8_prefix(data: &[u8]) -> &[u8] {
    match std::str::from_utf8(data) {
        Ok(_) => data,
        Err(error) => &data[..error.valid_up_to()],
    }
}

/// A capped collector fronted by UTF-8 normalization and secret redaction.
///
/// The three stages (UTF-8 normalization, exact redaction, capped collection)
/// are combined into one type because both call sites use the whole chain and a
/// chain of owned writers cannot be handed to the sandbox driver as a single
/// sink.
///
/// [`RedactingCollector::flush`] must be called once the stream ends; until
/// then a trailing partial rune and a trailing partial secret match are held
/// back deliberately.
#[derive(Debug)]
pub(crate) struct RedactingCollector {
    values: Vec<String>,
    marker: String,
    /// Bytes not yet known to form a complete rune.
    rune_pending: Vec<u8>,
    /// Normalized bytes not yet known to be outside a secret.
    match_pending: Vec<u8>,
    collector: CappedByteCollector,
}

impl RedactingCollector {
    /// `values` are the exact secrets to replace with `marker`. Empty and
    /// duplicate values are dropped; an empty `marker` deletes matches instead
    /// of replacing them.
    pub(crate) fn new(limit: usize, values: &[String], marker: &str) -> Self {
        let mut unique: Vec<String> = Vec::with_capacity(values.len());
        for value in values {
            if value.is_empty() || unique.iter().any(|seen| seen == value) {
                continue;
            }
            unique.push(value.clone());
        }
        Self {
            values: unique,
            marker: marker.to_owned(),
            rune_pending: Vec::new(),
            match_pending: Vec::new(),
            collector: CappedByteCollector::new(limit),
        }
    }

    pub(crate) fn write(&mut self, data: &[u8]) {
        self.rune_pending.extend_from_slice(data);
        self.normalize(false);
    }

    /// Emits everything held back: a trailing partial rune becomes `U+FFFD`
    /// and a trailing partial secret match is written out unredacted only if it
    /// is not a complete secret.
    pub(crate) fn flush(&mut self) {
        self.normalize(true);
        self.redact(true);
    }

    pub(crate) fn collector(&self) -> &CappedByteCollector {
        &self.collector
    }

    fn normalize(&mut self, final_chunk: bool) {
        while !self.rune_pending.is_empty() {
            if !final_chunk && !full_rune(&self.rune_pending) {
                break;
            }
            let (invalid, size) = decode_rune(&self.rune_pending);
            if invalid {
                let replacement = char::REPLACEMENT_CHARACTER.to_string();
                self.match_pending.extend_from_slice(replacement.as_bytes());
                self.rune_pending.drain(..1);
            } else {
                let head: Vec<u8> = self.rune_pending.drain(..size).collect();
                self.match_pending.extend_from_slice(&head);
            }
            self.redact(false);
        }
        self.redact(false);
    }

    fn redact(&mut self, final_chunk: bool) {
        while !self.match_pending.is_empty() {
            let Some((index, matched, unresolved)) = self.leftmost_candidate(final_chunk) else {
                let all: Vec<u8> = std::mem::take(&mut self.match_pending);
                self.collector.write(&all);
                return;
            };
            if index > 0 {
                let head: Vec<u8> = self.match_pending.drain(..index).collect();
                self.collector.write(&head);
                continue;
            }
            if unresolved {
                return;
            }
            if !self.marker.is_empty() {
                let marker = self.marker.clone();
                self.collector.write_atomic(marker.as_bytes());
            }
            self.match_pending.drain(..matched);
        }
    }

    /// Finds the earliest offset that either starts a secret or starts a
    /// prefix of one that the current chunk cannot yet rule out. When both
    /// hold at the same offset the unresolved candidate wins, so a longer
    /// secret is never missed by emitting a shorter one first.
    fn leftmost_candidate(&self, final_chunk: bool) -> Option<(usize, usize, bool)> {
        for index in 0..self.match_pending.len() {
            let remaining = &self.match_pending[index..];
            let mut longest = 0usize;
            let mut unresolved = false;
            for value in &self.values {
                let value = value.as_bytes();
                if remaining.starts_with(value) {
                    longest = longest.max(value.len());
                } else if !final_chunk
                    && remaining.len() < value.len()
                    && value.starts_with(remaining)
                {
                    unresolved = true;
                }
            }
            if longest > 0 || unresolved {
                return Some((index, longest, unresolved));
            }
        }
        None
    }
}

/// Lets the collector stand in as a sandbox stream sink. Writes never fail:
/// the cap discards rather than erroring, so the child never observes a
/// short write and a capture failure cannot be a distinct outcome.
impl std::io::Write for RedactingCollector {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        RedactingCollector::write(self, data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Replaces every configured secret in `value`, matching `redactExactText`.
/// An empty marker with a non-empty value set discards the text entirely,
/// because there is no safe way to show it.
pub(crate) fn redact_exact_text(value: &str, values: &[String], marker: &str) -> String {
    if marker.is_empty() && !values.is_empty() {
        return String::new();
    }
    let mut collector = RedactingCollector::new(usize::MAX, values, marker);
    collector.write(value.as_bytes());
    collector.flush();
    String::from_utf8_lossy(collector.collector().bytes()).into_owned()
}

/// Caps `content` at `max_output_bytes`, appending an omission marker,
/// matching `CappedTextResult`.
pub fn capped_text_result(content: &str, max_output_bytes: usize) -> ToolResult {
    let mut collector = CappedByteCollector::new(max_output_bytes);
    collector.write(content.as_bytes());
    if collector.discarded() == 0 {
        return ToolResult {
            content: content.to_owned(),
            ..ToolResult::default()
        };
    }
    let raw = collector.bytes();
    let safe = valid_utf8_prefix(raw);
    let omitted = collector.discarded() + raw.len() - safe.len();
    let text = if safe.is_empty() {
        format!("[truncated: {omitted} bytes omitted]")
    } else {
        format!(
            "{}\n[truncated: {omitted} bytes omitted]",
            String::from_utf8_lossy(safe)
        )
    };
    ToolResult {
        content: text,
        ..ToolResult::default()
    }
}

/// Renders a collector as a result, appending `marker` on its own line when
/// the output was truncated, matching `cappedCollectorResult`.
pub(crate) fn capped_collector_result(collector: &CappedByteCollector, marker: &str) -> ToolResult {
    let mut content = String::from_utf8_lossy(valid_utf8_prefix(collector.bytes())).into_owned();
    if !marker.is_empty() {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(marker);
        content.push('\n');
    }
    ToolResult {
        content,
        ..ToolResult::default()
    }
}

/// Decodes tool arguments, rejecting unknown fields, trailing tokens, and
/// missing required keys. The returned text is the message the model sees, so
/// it matches `encoding/json`'s wording.
pub(crate) fn decode_strict_json<T: DeserializeOwned>(
    arguments: &str,
    required: &[&str],
) -> Result<T, String> {
    let mut deserializer = serde_json::Deserializer::from_str(arguments);
    let decoded = match T::deserialize(&mut deserializer) {
        Ok(decoded) => decoded,
        Err(error) => {
            let text = error.to_string();
            return Err(match unknown_field_name(&text) {
                Some(field) => format!("json: unknown field {field:?}"),
                None => format!("invalid JSON: {text}"),
            });
        }
    };
    if deserializer.end().is_err() {
        return Err("trailing JSON tokens after arguments".to_owned());
    }
    if required.is_empty() {
        return Ok(decoded);
    }
    let provided: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(arguments).map_err(|error| format!("invalid JSON: {error}"))?;
    for key in required {
        if !provided.contains_key(*key) {
            return Err(format!("missing required argument: {key}"));
        }
    }
    Ok(decoded)
}

/// Extracts the field name from serde's unknown-field message so the caller can
/// render the `encoding/json` wording.
fn unknown_field_name(message: &str) -> Option<String> {
    let rest = message.strip_prefix("unknown field `")?;
    let end = rest.find('`')?;
    Some(rest[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_chunks(values: &[&str], chunks: &[&str]) -> String {
        let values: Vec<String> = values.iter().map(|value| (*value).to_owned()).collect();
        let mut collector = RedactingCollector::new(usize::MAX, &values, LEGACY_REDACTION_MARKER);
        for chunk in chunks {
            collector.write(chunk.as_bytes());
        }
        collector.flush();
        String::from_utf8_lossy(collector.collector().bytes()).into_owned()
    }

    #[test]
    fn the_collector_stops_at_the_first_incomplete_rune() {
        let mut collector = CappedByteCollector::new(1);
        collector.write("\u{a1}".as_bytes());
        collector.write(b"s");
        assert_eq!(collector.bytes(), b"");
        assert_eq!(collector.discarded(), "\u{a1}s".len());
    }

    #[test]
    fn redaction_is_leftmost_longest_across_every_split() {
        let cases: &[(&str, &[&str], String)] = &[
            (
                "credential-zzSHORT-rest",
                &["credential-zzSHORT-rest", "SHORT"],
                LEGACY_REDACTION_MARKER.to_owned(),
            ),
            (
                "before-abcdef-after",
                &["abc", "abcdef", "bcde", "def"],
                format!("before-{LEGACY_REDACTION_MARKER}-after"),
            ),
            (
                "xxTOKEN-tailTOKENyy",
                &[
                    "TOKEN",
                    "TOKEN-tail",
                    "tailTOKEN",
                    "OKEN",
                    "TOKEN-tailTOKEN",
                ],
                format!("xx{LEGACY_REDACTION_MARKER}yy"),
            ),
            (
                "zababaq",
                &["aba", "bab", "ababa"],
                format!("z{LEGACY_REDACTION_MARKER}q"),
            ),
        ];
        for (input, values, expected) in cases {
            for split in 0..=input.len() {
                let got = redact_chunks(values, &[&input[..split], &input[split..]]);
                assert_eq!(&got, expected, "input {input:?} split at {split}");
            }
            let chunks: Vec<&str> = (0..input.len())
                .map(|index| &input[index..index + 1])
                .collect();
            assert_eq!(&redact_chunks(values, &chunks), expected, "byte by byte");
        }
    }

    #[test]
    fn a_same_start_longer_candidate_is_held_until_flush() {
        let values = vec!["SHORT".to_owned(), "SHORT-rest".to_owned()];
        let mut collector = RedactingCollector::new(usize::MAX, &values, LEGACY_REDACTION_MARKER);
        collector.write(b"SHORT");
        assert_eq!(collector.collector().bytes(), b"");
        collector.flush();
        assert_eq!(
            collector.collector().bytes(),
            LEGACY_REDACTION_MARKER.as_bytes()
        );
    }

    #[test]
    fn invalid_utf8_becomes_the_replacement_character() {
        let mut collector = RedactingCollector::new(usize::MAX, &[], "");
        collector.write(&[0x66, 0xff, 0x6f]);
        collector.flush();
        assert_eq!(
            String::from_utf8_lossy(collector.collector().bytes()),
            "f\u{fffd}o"
        );
    }

    #[test]
    fn an_empty_marker_deletes_matches_but_only_when_values_exist() {
        assert_eq!(redact_exact_text("keep", &[], ""), "keep");
        assert_eq!(redact_exact_text("keep", &["k".to_owned()], ""), "");
    }

    #[test]
    fn capped_text_result_appends_the_omission_marker() {
        let untouched = capped_text_result("hello", 100);
        assert_eq!(untouched.content, "hello");
        assert!(!untouched.is_error);

        let truncated = capped_text_result("hello world", 5);
        assert_eq!(truncated.content, "hello\n[truncated: 6 bytes omitted]");

        let nothing_kept = capped_text_result("hello", 0);
        assert_eq!(nothing_kept.content, "[truncated: 5 bytes omitted]");
    }

    #[test]
    fn capped_collector_result_puts_the_marker_on_its_own_line() {
        let mut collector = CappedByteCollector::new(100);
        collector.write(b"a\n");
        assert_eq!(
            capped_collector_result(&collector, "[truncated: result limit reached]").content,
            "a\n[truncated: result limit reached]\n"
        );

        let mut unterminated = CappedByteCollector::new(100);
        unterminated.write(b"a");
        assert_eq!(
            capped_collector_result(&unterminated, "[truncated: output limit reached]").content,
            "a\n[truncated: output limit reached]\n"
        );

        assert_eq!(capped_collector_result(&unterminated, "").content, "a");
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        #[serde(default)]
        path: String,
    }

    #[test]
    fn strict_decoding_reports_the_fixed_error_text() {
        let ok: Args = decode_strict_json(r#"{"path":"a"}"#, &["path"]).unwrap();
        assert_eq!(ok.path, "a");

        let unknown = decode_strict_json::<Args>(r#"{"path":"a","nope":1}"#, &[]).unwrap_err();
        assert_eq!(unknown, r#"json: unknown field "nope""#);

        let trailing = decode_strict_json::<Args>(r#"{"path":"a"} {}"#, &[]).unwrap_err();
        assert_eq!(trailing, "trailing JSON tokens after arguments");

        let missing = decode_strict_json::<Args>("{}", &["path"]).unwrap_err();
        assert_eq!(missing, "missing required argument: path");

        let malformed = decode_strict_json::<Args>("{", &[]).unwrap_err();
        assert!(malformed.starts_with("invalid JSON: "), "{malformed}");
    }
}
