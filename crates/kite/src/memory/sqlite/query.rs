//! SQL fragments shared by the list and retrieval paths.
//!
//! Every projected column is wrapped in a safety `CASE`, so a row whose stored
//! shape is wrong reaches the decoder as NULL and is reported as corruption
//! instead of being silently coerced.

use rusqlite::types::Value;

use super::codec::TIMESTAMP_BYTES;
use crate::memory::{
    Error, ListRequest, MAX_BASELINE_RECORDS, MAX_FTS_TERM_BYTES, MAX_FTS_TERMS, MAX_ID_BYTES,
    MAX_KIND_BYTES, MAX_LABEL_BYTES, MAX_LABELS, MAX_METADATA_BYTES, MAX_NAMESPACE_BYTES,
    MAX_QUERY_BYTES, MAX_RECORD_TEXT_BYTES, MAX_RETRIEVAL_CANDIDATES, MAX_SCOPE_ID_BYTES,
    MAX_SEMANTIC_KEY_BYTES, Result, RetrievalRequest, Scope, invalid_request,
};

use super::codec::{MAX_LABELS_JSON_BYTES, MAX_SOURCE_JSON_BYTES, format_timestamp};

/// Whether a rune makes a token run worth searching for.
///
/// A full answer would ask for Letter, Mark, Number or Symbol. Rust's std has
/// no Mark or Symbol predicate, so this covers Letter and Number exactly and
/// Symbol only across ASCII. A run made entirely of non-ASCII marks or symbols
/// is therefore dropped; every run containing a letter or digit behaves
/// identically.
fn meaningful(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character,
            '$' | '+' | '<' | '=' | '>' | '^' | '`' | '|' | '~'
        )
}

fn quote_fts_term(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Converts free text into an FTS5 expression of quoted literals joined by
/// `OR`. Quoting every term means user text can never become FTS syntax.
pub fn build_fts_literal_expression(input: &str) -> Result<String> {
    if input.len() > MAX_QUERY_BYTES {
        return Err(invalid_request("lexical query bounds"));
    }
    let mut terms: Vec<String> = Vec::new();
    let mut start: Option<usize> = None;
    let mut keep = false;
    let mut flush = |end: usize, start: &mut Option<usize>, keep: &mut bool| -> Result<()> {
        let Some(begin) = start.take() else {
            return Ok(());
        };
        if !*keep {
            return Ok(());
        }
        *keep = false;
        let term = &input[begin..end];
        if term.len() > MAX_FTS_TERM_BYTES {
            return Err(invalid_request("lexical term bounds"));
        }
        if terms.len() >= MAX_FTS_TERMS {
            return Err(invalid_request("lexical term count"));
        }
        terms.push(quote_fts_term(term));
        Ok(())
    };
    for (offset, character) in input.char_indices() {
        if character.is_whitespace() || character.is_control() {
            flush(offset, &mut start, &mut keep)?;
            continue;
        }
        if start.is_none() {
            start = Some(offset);
        }
        if meaningful(character) {
            keep = true;
        }
    }
    flush(input.len(), &mut start, &mut keep)?;
    Ok(terms.join(" OR "))
}

fn text_safety(qualified: &str, minimum: usize, maximum: usize) -> String {
    format!(
        "typeof({qualified})='text' AND length(CAST({qualified} AS BLOB)) BETWEEN {minimum} AND {maximum}"
    )
}

/// The safety predicate for one record row. `alias` is `""` for an unqualified
/// `memory_records` scan or `"r."` when the table is joined under an alias.
pub fn record_safety(alias: &str) -> String {
    let column = |name: &str| format!("{alias}{name}");
    let text =
        |name: &str, minimum: usize, maximum: usize| text_safety(&column(name), minimum, maximum);
    let bounded = |name: &str, maximum: usize| text_safety(&column(name), 0, maximum);
    let json_value = |name: &str, maximum: usize, kind: &str| {
        let qualified = column(name);
        let gate = bounded(name, maximum);
        let projected = format!("CASE WHEN {gate} THEN {qualified} END");
        format!("{gate} AND json_valid({projected}) AND json_type({projected})='{kind}'")
    };
    [
        text("id", 1, MAX_ID_BYTES),
        text("scope_namespace", 1, MAX_NAMESPACE_BYTES),
        text("scope_id", 1, MAX_SCOPE_ID_BYTES),
        text("kind", 1, MAX_KIND_BYTES),
        bounded("semantic_key", MAX_SEMANTIC_KEY_BYTES),
        text("text_value", 1, MAX_RECORD_TEXT_BYTES),
        json_value("labels_json", MAX_LABELS_JSON_BYTES, "array"),
        json_value("metadata_json", MAX_METADATA_BYTES, "object"),
        json_value("source_json", MAX_SOURCE_JSON_BYTES, "object"),
        format!("typeof({}) IN ('real','integer')", column("confidence")),
        format!(
            "typeof({revision})='integer' AND {revision} BETWEEN 1 AND {max}",
            revision = column("revision"),
            max = i64::MAX
        ),
        text("created_at", TIMESTAMP_BYTES, TIMESTAMP_BYTES),
        text("updated_at", TIMESTAMP_BYTES, TIMESTAMP_BYTES),
        format!(
            "({expires} IS NULL OR ({safe}))",
            expires = column("expires_at"),
            safe = text("expires_at", TIMESTAMP_BYTES, TIMESTAMP_BYTES)
        ),
        format!(
            "typeof({state})='text' AND {state}='active'",
            state = column("state")
        ),
        format!("{} IS NULL", column("forgotten_at")),
    ]
    .join(" AND ")
}

pub const RECORD_COLUMNS: [&str; 14] = [
    "id",
    "scope_namespace",
    "scope_id",
    "kind",
    "semantic_key",
    "text_value",
    "labels_json",
    "metadata_json",
    "source_json",
    "confidence",
    "revision",
    "created_at",
    "updated_at",
    "expires_at",
];

/// A validity flag followed by the fourteen gated record columns.
pub fn record_projection(alias: &str) -> String {
    let safety = record_safety(alias);
    let mut parts = vec![format!("CASE WHEN {safety} THEN 1 ELSE 0 END")];
    for column in RECORD_COLUMNS {
        parts.push(format!("CASE WHEN {safety} THEN {alias}{column} END"));
    }
    parts.join(",")
}

pub fn tombstone_safety() -> String {
    [
        text_safety("id", 1, MAX_ID_BYTES),
        text_safety("scope_namespace", 1, MAX_NAMESPACE_BYTES),
        text_safety("scope_id", 1, MAX_SCOPE_ID_BYTES),
        format!("typeof(revision)='integer' AND revision BETWEEN 1 AND {}", i64::MAX),
        format!("typeof(created_at)='text' AND length(CAST(created_at AS BLOB))={TIMESTAMP_BYTES}"),
        format!("typeof(updated_at)='text' AND length(CAST(updated_at AS BLOB))={TIMESTAMP_BYTES}"),
        format!("typeof(forgotten_at)='text' AND length(CAST(forgotten_at AS BLOB))={TIMESTAMP_BYTES}"),
        "typeof(state)='text' AND state='tombstone'".to_string(),
        "typeof(kind)='text' AND length(CAST(kind AS BLOB))=0".to_string(),
        "typeof(semantic_key)='text' AND length(CAST(semantic_key AS BLOB))=0".to_string(),
        "typeof(text_value)='text' AND length(CAST(text_value AS BLOB))=0".to_string(),
        "typeof(labels_json)='text' AND length(CAST(labels_json AS BLOB))=2 AND labels_json='[]'".to_string(),
        "typeof(metadata_json)='text' AND length(CAST(metadata_json AS BLOB))=2 AND metadata_json='{}'".to_string(),
        "typeof(source_json)='text' AND length(CAST(source_json AS BLOB))=2 AND source_json='{}'".to_string(),
        "typeof(confidence) IN ('real','integer') AND confidence=0".to_string(),
        "expires_at IS NULL".to_string(),
        "forgotten_at=updated_at".to_string(),
    ]
    .join(" AND ")
}

pub fn tombstone_projection() -> String {
    let safety = tombstone_safety();
    let mut parts = vec![format!("CASE WHEN {safety} THEN 1 ELSE 0 END")];
    for column in [
        "id",
        "scope_namespace",
        "scope_id",
        "revision",
        "created_at",
        "updated_at",
        "forgotten_at",
    ] {
        parts.push(format!("CASE WHEN {safety} THEN {column} END"));
    }
    parts.join(",")
}

pub fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

/// A SQL predicate that is true only for a canonically formatted timestamp,
/// including the correct last day of the month. Comparing timestamps as text
/// is only valid once this holds.
pub fn timestamp_predicate_safety(column: &str) -> String {
    let value = format!("CAST({column} AS TEXT)");
    let pattern = "[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]Z";
    let year = format!("CAST(substr({value},1,4) AS INTEGER)");
    let month = format!("CAST(substr({value},6,2) AS INTEGER)");
    let day = format!("CAST(substr({value},9,2) AS INTEGER)");
    let leap = format!("(({year}%4=0 AND {year}%100<>0) OR {year}%400=0)");
    let last_day = format!(
        "CASE {month} WHEN 2 THEN CASE WHEN {leap} THEN 29 ELSE 28 END WHEN 4 THEN 30 WHEN 6 THEN 30 WHEN 9 THEN 30 WHEN 11 THEN 30 ELSE 31 END"
    );
    format!(
        "typeof({column})='text' AND length(CAST({column} AS BLOB))={TIMESTAMP_BYTES} \
AND {value} GLOB '{pattern}' \
AND substr({value},5,1)='-' AND substr({value},8,1)='-' \
AND substr({value},11,1)='T' AND substr({value},14,1)=':' \
AND substr({value},17,1)=':' AND substr({value},20,1)='.' AND substr({value},30,1)='Z' \
AND {year} BETWEEN 1 AND 9999 AND {month} BETWEEN 1 AND 12 \
AND {day} BETWEEN 1 AND ({last_day}) \
AND CAST(substr({value},12,2) AS INTEGER) BETWEEN 0 AND 23 \
AND CAST(substr({value},15,2) AS INTEGER) BETWEEN 0 AND 59 \
AND CAST(substr({value},18,2) AS INTEGER) BETWEEN 0 AND 59"
    )
}

/// One built statement and its bound arguments.
pub struct Statement {
    pub sql: String,
    pub arguments: Vec<Value>,
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

pub fn build_list_query(
    request: &ListRequest,
    cursor: Option<&super::cursor::RecordCursor>,
) -> Statement {
    let mut clauses = vec!["state='active'".to_string()];
    let mut arguments: Vec<Value> = Vec::new();
    let mut scopes = Vec::new();
    for scope in &request.scopes {
        scopes.push("(scope_namespace=? AND scope_id=?)".to_string());
        arguments.push(text(&scope.namespace));
        arguments.push(text(&scope.id));
    }
    clauses.push(format!("({})", scopes.join(" OR ")));
    if !request.kinds.is_empty() {
        clauses.push(format!("kind IN ({})", placeholders(request.kinds.len())));
        arguments.extend(request.kinds.iter().map(|kind| text(kind)));
    }
    let label_gate = format!(
        "typeof(labels_json)='text' AND length(CAST(labels_json AS BLOB))<={MAX_LABELS_JSON_BYTES}"
    );
    let gated = format!("CASE WHEN {label_gate} THEN labels_json END");
    let valid = format!("{label_gate} AND json_valid({gated})");
    let iterable =
        format!("CASE WHEN {valid} AND json_type({gated})='array' THEN {gated} ELSE '[]' END");
    let shape = format!(
        "{valid} AND json_type({gated})='array' AND NOT EXISTS (SELECT 1 FROM json_each({iterable}) WHERE type<>'text')"
    );
    for label in &request.labels {
        // Structurally unsafe label JSON must still reach the bounded
        // projection so it is reported as corruption rather than filtered away.
        clauses.push(format!(
            "(NOT ({shape}) OR EXISTS (SELECT 1 FROM json_each({iterable}) WHERE type='text' AND value=?))"
        ));
        arguments.push(text(label));
    }
    if !request.include_expired {
        let safe = format!(
            "{} AND {} AND expires_at>=created_at",
            timestamp_predicate_safety("expires_at"),
            timestamp_predicate_safety("created_at")
        );
        clauses.push(format!(
            "(expires_at IS NULL OR NOT ({safe}) OR expires_at>?)"
        ));
        arguments.push(text(&format_timestamp(request.now)));
    }
    if let Some(cursor) = cursor {
        clauses.push("(updated_at<? OR (updated_at=? AND id>?))".to_string());
        arguments.push(text(&cursor.updated_at));
        arguments.push(text(&cursor.updated_at));
        arguments.push(text(&cursor.id));
    }
    arguments.push(Value::Integer(request.limit as i64 + 1));
    Statement {
        sql: format!(
            "SELECT {} FROM memory_records WHERE {} ORDER BY updated_at DESC,id ASC LIMIT ?",
            record_projection(""),
            clauses.join(" AND ")
        ),
        arguments,
    }
}

fn retrieval_filter(request: &RetrievalRequest, scopes: &[Scope]) -> (Vec<String>, Vec<Value>) {
    let mut clauses = vec!["r.state=?".to_string()];
    let mut arguments = vec![text("active")];
    let mut scope_clauses = Vec::new();
    for scope in scopes {
        scope_clauses.push("(r.scope_namespace=? AND r.scope_id=?)".to_string());
        arguments.push(text(&scope.namespace));
        arguments.push(text(&scope.id));
    }
    clauses.push(format!("({})", scope_clauses.join(" OR ")));
    if !request.kinds.is_empty() {
        clauses.push(format!("r.kind IN ({})", placeholders(request.kinds.len())));
        arguments.extend(request.kinds.iter().map(|kind| text(kind)));
    }
    if !request.include_expired {
        let safe = format!(
            "{} AND {} AND r.expires_at>=r.created_at",
            timestamp_predicate_safety("r.expires_at"),
            timestamp_predicate_safety("r.created_at")
        );
        clauses.push(format!(
            "(r.expires_at IS NULL OR NOT ({safe}) OR r.expires_at>?)"
        ));
        arguments.push(text(&format_timestamp(request.now)));
    }
    (clauses, arguments)
}

/// The records every recall starts from: the user's own keyed preferences and
/// instructions, newest first, regardless of the query text.
pub fn build_baseline_query(request: &RetrievalRequest, user: &Scope) -> Statement {
    let (mut clauses, mut arguments) = retrieval_filter(request, std::slice::from_ref(user));
    clauses.push("r.semantic_key<>?".to_string());
    clauses.push("r.kind IN (?,?)".to_string());
    arguments.push(text(""));
    arguments.push(text("preference"));
    arguments.push(text("instruction"));
    arguments.push(Value::Integer(MAX_BASELINE_RECORDS as i64));
    Statement {
        sql: format!(
            "SELECT {} FROM memory_records r WHERE {} ORDER BY r.updated_at DESC,r.id ASC LIMIT ?",
            record_projection("r."),
            clauses.join(" AND ")
        ),
        arguments,
    }
}

const RANK: &str = "bm25(memory_records_fts,0.0,1.0,0.5,2.0,1.0)";

pub fn build_fts_candidate_query(request: &RetrievalRequest, expression: &str) -> Statement {
    let (mut clauses, mut arguments) = retrieval_filter(request, &request.scopes);
    clauses.push("memory_records_fts MATCH ?".to_string());
    arguments.push(text(expression));
    arguments.push(Value::Integer(MAX_RETRIEVAL_CANDIDATES as i64));
    Statement {
        sql: format!(
            "SELECT {projection},{fts},CASE WHEN typeof({RANK}) IN ('real','integer') THEN {RANK} END \
FROM memory_records_fts JOIN memory_records r ON memory_records_fts.record_id=r.id WHERE {where_clause} \
ORDER BY {RANK} ASC,r.updated_at DESC,r.id ASC LIMIT ?",
            projection = record_projection("r."),
            fts = fts_projection(),
            where_clause = clauses.join(" AND ")
        ),
        arguments,
    }
}

/// A `(kind, key)` pair whose user-scope record a workspace record replaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RetrievalKey {
    pub kind: String,
    pub key: String,
}

pub fn build_workspace_replacement_query(
    request: &RetrievalRequest,
    workspace: &Scope,
    keys: &[RetrievalKey],
) -> Statement {
    let mut arguments: Vec<Value> = Vec::new();
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        values.push("(?,?)");
        arguments.push(text(&key.kind));
        arguments.push(text(&key.key));
    }
    let (clauses, filter_arguments) = retrieval_filter(request, std::slice::from_ref(workspace));
    arguments.extend(filter_arguments);
    arguments.push(Value::Integer(MAX_RETRIEVAL_CANDIDATES as i64));
    Statement {
        sql: format!(
            "WITH requested(kind,key) AS (VALUES {}) SELECT {} FROM memory_records r \
JOIN requested q ON q.kind=r.kind AND q.key=r.semantic_key WHERE {} \
ORDER BY r.updated_at DESC,r.id ASC LIMIT ?",
            values.join(","),
            record_projection("r."),
            clauses.join(" AND ")
        ),
        arguments,
    }
}

/// The FTS-side columns the retriever reads back, each behind its own gate.
pub fn fts_projection() -> String {
    let gate = |column: &str, minimum: usize, maximum: usize| {
        let qualified = format!("memory_records_fts.{column}");
        format!(
            "CASE WHEN typeof({qualified})='text' AND length(CAST({qualified} AS BLOB)) BETWEEN {minimum} AND {maximum} THEN {qualified} END"
        )
    };
    let max_labels_bytes = MAX_LABELS * MAX_LABEL_BYTES + MAX_LABELS - 1;
    [
        gate("record_id", 1, MAX_ID_BYTES),
        gate("text_value", 1, MAX_RECORD_TEXT_BYTES),
        gate("kind", 1, MAX_KIND_BYTES),
        gate("semantic_key", 0, MAX_SEMANTIC_KEY_BYTES),
        gate("labels", 0, max_labels_bytes),
    ]
    .join(",")
}

/// Guards against an `Error` being constructed for a query too large to build.
pub fn ensure_buildable(keys: usize) -> Result<()> {
    if keys == 0 {
        return Err(Error::new(crate::memory::ErrorKind::InvalidRequest));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_text_becomes_quoted_literals_joined_by_or() {
        assert_eq!(
            build_fts_literal_expression("hello world").expect("build"),
            r#""hello" OR "world""#
        );
    }

    #[test]
    fn fts_syntax_in_user_text_is_quoted_rather_than_interpreted() {
        let expression = build_fts_literal_expression(r#"a" OR b NEAR c"#).expect("build");
        assert!(expression.starts_with(r#""a""" OR "#), "got {expression}");
        assert!(
            expression.contains(r#""NEAR""#),
            "NEAR was not quoted: {expression}"
        );
    }

    #[test]
    fn punctuation_only_runs_are_dropped() {
        assert_eq!(build_fts_literal_expression("... ,,,").expect("build"), "");
        assert_eq!(build_fts_literal_expression("").expect("build"), "");
    }

    #[test]
    fn an_oversized_query_is_an_invalid_request() {
        let long = "a".repeat(MAX_QUERY_BYTES + 1);
        assert!(build_fts_literal_expression(&long).is_err());
        let long_term = "b".repeat(MAX_FTS_TERM_BYTES + 1);
        assert!(build_fts_literal_expression(&long_term).is_err());
        let many = vec!["x"; MAX_FTS_TERMS + 1].join(" ");
        assert!(build_fts_literal_expression(&many).is_err());
    }

    #[test]
    fn the_record_projection_gates_every_column() {
        let safety = record_safety("");
        let projection = record_projection("");
        assert_eq!(
            projection
                .matches(&format!("CASE WHEN {safety} THEN"))
                .count(),
            RECORD_COLUMNS.len() + 1
        );
    }

    #[test]
    fn the_timestamp_predicate_pins_the_last_day_of_february() {
        let predicate = timestamp_predicate_safety("expires_at");
        assert!(predicate.contains("WHEN 2 THEN CASE WHEN"));
        assert!(predicate.contains("%400=0"));
    }
}
