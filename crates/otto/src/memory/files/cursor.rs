//! Opaque list cursors.
//!
//! A cursor carries the fingerprint of the query that produced it, so resuming
//! a list with changed filters is rejected instead of silently skipping or
//! repeating rows. The file store has no generation; a list cursor is a keyset
//! position and stays valid across writes.

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::memory::codec::{format_timestamp, parse_timestamp};
use crate::memory::json::encode_string;
use crate::memory::{
    CandidateListRequest, Error, ErrorKind, ListRequest, MAX_CURSOR_BYTES, MAX_ID_BYTES,
    MAX_RETRIEVAL_CANDIDATES, Result, RetrievalRequest, Scope,
};

const RECORD_CURSOR_VERSION: i64 = 1;
const MAX_DECODED_CURSOR_BYTES: usize = 3 * 1024;

/// The decoded position of a list cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RecordCursor {
    pub(super) fingerprint: String,
    pub(super) updated_at: String,
    pub(super) id: String,
}

fn invalid() -> Error {
    Error::new(ErrorKind::InvalidCursor)
}

/// An absent slice is encoded as `null`, and the fingerprint is a digest over
/// those exact bytes.
fn encode_scopes(scopes: &[Scope]) -> String {
    if scopes.is_empty() {
        return "null".to_string();
    }
    let mut sorted: Vec<&Scope> = scopes.iter().collect();
    sorted.sort_by(|left, right| (&left.namespace, &left.id).cmp(&(&right.namespace, &right.id)));
    let mut out = String::from("[");
    for (index, scope) in sorted.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"Namespace\":");
        encode_string(&scope.namespace, &mut out);
        out.push_str(",\"ID\":");
        encode_string(&scope.id, &mut out);
        out.push('}');
    }
    out.push(']');
    out
}

fn encode_sorted_strings(values: &[String]) -> String {
    if values.is_empty() {
        return "null".to_string();
    }
    let mut sorted: Vec<&str> = values.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut out = String::from("[");
    for (index, value) in sorted.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        encode_string(value, &mut out);
    }
    out.push(']');
    out
}

fn digest(canonical: &str) -> String {
    Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `now` participates only when expired records are excluded, because that is
/// the only case where it changes the result set.
pub(super) fn fingerprint_list(request: &ListRequest) -> String {
    let mut canonical = String::from("{\"domain\":\"records\",\"all_scopes\":");
    canonical.push_str(if request.all_scopes { "true" } else { "false" });
    canonical.push_str(",\"scopes\":");
    canonical.push_str(&encode_scopes(&request.scopes));
    canonical.push_str(",\"kinds\":");
    canonical.push_str(&encode_sorted_strings(&request.kinds));
    canonical.push_str(",\"labels\":");
    canonical.push_str(&encode_sorted_strings(&request.labels));
    canonical.push_str(",\"include_expired\":");
    canonical.push_str(if request.include_expired {
        "true"
    } else {
        "false"
    });
    if !request.include_expired {
        canonical.push_str(",\"now\":");
        encode_string(&format_timestamp(request.now), &mut canonical);
    }
    canonical.push('}');
    digest(&canonical)
}

pub(super) fn fingerprint_candidates(request: &CandidateListRequest) -> String {
    let states: BTreeSet<&'static str> = request.states.iter().map(|s| s.as_str()).collect();
    let mut canonical = String::from("{\"domain\":\"candidates\",\"scopes\":");
    canonical.push_str(&encode_scopes(&request.scopes));
    canonical.push_str(",\"kinds\":null,\"labels\":null");
    if !states.is_empty() {
        canonical.push_str(",\"states\":[");
        for (index, state) in states.iter().enumerate() {
            if index > 0 {
                canonical.push(',');
            }
            encode_string(state, &mut canonical);
        }
        canonical.push(']');
    }
    canonical.push_str(",\"include_expired\":false}");
    digest(&canonical)
}

pub(super) fn encode_record_cursor(
    fingerprint: &str,
    updated_at: &str,
    id: &str,
) -> Result<String> {
    let mut raw = String::from("{\"v\":1,\"fingerprint\":");
    encode_string(fingerprint, &mut raw);
    raw.push_str(",\"updated_at\":");
    encode_string(updated_at, &mut raw);
    raw.push_str(",\"id\":");
    encode_string(id, &mut raw);
    raw.push('}');
    if raw.len() > MAX_DECODED_CURSOR_BYTES {
        return Err(invalid());
    }
    let encoded = URL_SAFE_NO_PAD.encode(raw.as_bytes());
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    Ok(encoded)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorWire {
    v: i64,
    fingerprint: String,
    updated_at: String,
    id: String,
}

/// Returns `None` for an empty cursor, meaning "start at the beginning".
pub(super) fn decode_record_cursor(value: &str, fingerprint: &str) -> Result<Option<RecordCursor>> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    let raw = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    if raw.len() > MAX_DECODED_CURSOR_BYTES {
        return Err(invalid());
    }
    let raw = String::from_utf8(raw).map_err(|_| invalid())?;
    let wire: CursorWire = serde_json::from_str(&raw).map_err(|_| invalid())?;
    if wire.v != RECORD_CURSOR_VERSION || wire.fingerprint != fingerprint {
        return Err(invalid());
    }
    parse_timestamp(&wire.updated_at).map_err(|_| invalid())?;
    if !valid_cursor_id(&wire.id) {
        return Err(invalid());
    }
    let cursor = RecordCursor {
        fingerprint: wire.fingerprint,
        updated_at: wire.updated_at,
        id: wire.id,
    };
    // The decoded payload is re-encoded and anything not byte-identical is
    // rejected, which is what makes the cursor genuinely opaque.
    let canonical = encode_record_cursor(&cursor.fingerprint, &cursor.updated_at, &cursor.id)?;
    if canonical != value {
        return Err(invalid());
    }
    Ok(Some(cursor))
}

/// The decoded position of a retrieval cursor. Retrieval pages by ordinal
/// inside one ranked snapshot rather than by `(updated_at, id)`, because the
/// ranking is not a single column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RetrievalCursor {
    pub(super) fingerprint: String,
    pub(super) ordinal: usize,
    pub(super) id: String,
}

pub(super) fn fingerprint_retrieval(request: &RetrievalRequest) -> String {
    let mut canonical = String::from("{\"domain\":\"retrieval\",\"query_hash\":");
    encode_string(&digest(&request.query), &mut canonical);
    canonical.push_str(",\"scopes\":");
    canonical.push_str(&encode_scopes(&request.scopes));
    canonical.push_str(",\"kinds\":");
    canonical.push_str(&encode_sorted_strings(&request.kinds));
    canonical.push_str(",\"labels\":");
    canonical.push_str(&encode_sorted_strings(&request.labels));
    canonical.push_str(",\"now\":");
    encode_string(&format_timestamp(request.now), &mut canonical);
    canonical.push_str(",\"include_expired\":");
    canonical.push_str(if request.include_expired {
        "true"
    } else {
        "false"
    });
    canonical.push_str(",\"include_baseline\":");
    canonical.push_str(if request.include_baseline {
        "true"
    } else {
        "false"
    });
    canonical.push_str(&format!(
        ",\"limit\":{},\"token_budget\":{}}}",
        request.limit, request.token_budget
    ));
    digest(&canonical)
}

pub(super) fn encode_retrieval_cursor(
    fingerprint: &str,
    ordinal: usize,
    id: &str,
) -> Result<String> {
    if !(1..=MAX_RETRIEVAL_CANDIDATES).contains(&ordinal) || !valid_cursor_id(id) {
        return Err(invalid());
    }
    let mut raw = String::from("{\"v\":1,\"fingerprint\":");
    encode_string(fingerprint, &mut raw);
    raw.push_str(&format!(",\"ordinal\":{ordinal},\"id\":"));
    encode_string(id, &mut raw);
    raw.push('}');
    if raw.len() > MAX_DECODED_CURSOR_BYTES {
        return Err(invalid());
    }
    let encoded = URL_SAFE_NO_PAD.encode(raw.as_bytes());
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    Ok(encoded)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetrievalCursorWire {
    v: i64,
    fingerprint: String,
    ordinal: usize,
    id: String,
}

/// Returns `None` for an empty cursor, meaning "start at the beginning".
pub(super) fn decode_retrieval_cursor(
    value: &str,
    fingerprint: &str,
) -> Result<Option<RetrievalCursor>> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    let raw = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    if raw.len() > MAX_DECODED_CURSOR_BYTES {
        return Err(invalid());
    }
    let raw = String::from_utf8(raw).map_err(|_| invalid())?;
    let wire: RetrievalCursorWire = serde_json::from_str(&raw).map_err(|_| invalid())?;
    if wire.v != RECORD_CURSOR_VERSION || wire.fingerprint != fingerprint {
        return Err(invalid());
    }
    let cursor = RetrievalCursor {
        fingerprint: wire.fingerprint,
        ordinal: wire.ordinal,
        id: wire.id,
    };
    if encode_retrieval_cursor(&cursor.fingerprint, cursor.ordinal, &cursor.id)? != value {
        return Err(invalid());
    }
    Ok(Some(cursor))
}

pub(super) fn valid_cursor_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::CandidateState;
    use chrono::DateTime;

    fn list_request() -> ListRequest {
        ListRequest {
            all_scopes: false,
            scopes: vec![Scope::new("user", "u1")],
            kinds: vec!["preference".into()],
            labels: Vec::new(),
            limit: 10,
            cursor: String::new(),
            now: DateTime::UNIX_EPOCH,
            include_expired: false,
        }
    }

    #[test]
    fn a_cursor_round_trips_against_its_own_fingerprint() {
        let fingerprint = fingerprint_list(&list_request());
        let encoded = encode_record_cursor(&fingerprint, "2026-09-13T00:00:00.000000000Z", "rec-1")
            .expect("encode");
        let decoded = decode_record_cursor(&encoded, &fingerprint)
            .expect("decode")
            .expect("some");
        assert_eq!(decoded.id, "rec-1");
    }

    #[test]
    fn a_cursor_from_a_different_query_is_rejected() {
        let fingerprint = fingerprint_list(&list_request());
        let encoded = encode_record_cursor(&fingerprint, "2026-09-13T00:00:00.000000000Z", "r")
            .expect("encode");
        let mut other = list_request();
        other.kinds = vec!["instruction".into()];
        let error = decode_record_cursor(&encoded, &fingerprint_list(&other)).expect_err("reject");
        assert!(error.is(ErrorKind::InvalidCursor));
    }

    #[test]
    fn an_empty_cursor_means_the_first_page() {
        assert_eq!(decode_record_cursor("", "any").expect("ok"), None);
    }

    #[test]
    fn garbage_is_an_invalid_cursor_rather_than_a_panic() {
        for value in ["!!!", "AAAA", "a"] {
            assert!(
                decode_record_cursor(value, "f").is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn filter_order_does_not_change_the_fingerprint() {
        let mut first = list_request();
        first.labels = vec!["b".into(), "a".into()];
        let mut second = list_request();
        second.labels = vec!["a".into(), "b".into()];
        assert_eq!(fingerprint_list(&first), fingerprint_list(&second));
    }

    #[test]
    fn candidate_and_record_fingerprints_differ() {
        let candidates = CandidateListRequest {
            scopes: vec![Scope::new("user", "u1")],
            states: vec![CandidateState::Pending],
            limit: 10,
            cursor: String::new(),
        };
        assert_ne!(
            fingerprint_candidates(&candidates),
            fingerprint_list(&list_request())
        );
    }

    #[test]
    fn cursor_ids_allow_only_the_stored_id_alphabet() {
        assert!(valid_cursor_id("abc-1_2.3:4"));
        assert!(!valid_cursor_id(""));
        assert!(!valid_cursor_id("has space"));
    }
}
