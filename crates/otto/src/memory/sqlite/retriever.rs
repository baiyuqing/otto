//! Ranked retrieval.
//!
//! One request reads a single snapshot: an optional baseline set, the FTS
//! matches, and the workspace records that override user records with the same
//! `(kind, key)`. The result is ranked in Rust rather than in SQL, so the page
//! cursor carries an ordinal into that snapshot and is invalidated by any
//! write.

use rusqlite::{Connection, Row, params_from_iter};
use sha2::{Digest, Sha256};

use super::codec::fts_labels;
use super::cursor::{decode_retrieval_cursor, encode_retrieval_cursor, fingerprint_retrieval};
use super::query::{
    RetrievalKey, Statement, build_baseline_query, build_fts_candidate_query,
    build_fts_literal_expression, build_workspace_replacement_query,
};
use super::records::{decode_record_row, in_read_transaction, read_generation};
use super::{Store, map_sqlite_error};
use crate::memory::guard::guard_record;
use crate::memory::json::encode_string;
use crate::memory::validate::validate_retrieval_request;
use crate::memory::{
    Error, ErrorKind, MAX_BASELINE_RECORDS, MAX_RETRIEVAL_CANDIDATES, NAMESPACE_USER,
    NAMESPACE_WORKSPACE, Record, Result, RetrievalMatch, RetrievalRequest, RetrievalResult, Scope,
    TokenEstimator,
};

/// One record in the ranked snapshot. `lexical` is the record's 1-based place
/// in the FTS ordering, or [`usize::MAX`] for a baseline-only record.
#[derive(Debug, Clone)]
struct RetrievalCandidate {
    record: Record,
    baseline: bool,
    workspace: bool,
    lexical: usize,
}

fn corrupt() -> Error {
    Error::new(ErrorKind::Corrupt)
}

fn invalid_cursor() -> Error {
    Error::new(ErrorKind::InvalidCursor)
}

fn make_candidate(record: Record, baseline: bool, lexical: usize) -> RetrievalCandidate {
    let workspace = record.scope.namespace == NAMESPACE_WORKSPACE;
    RetrievalCandidate {
        record,
        baseline,
        workspace,
        lexical,
    }
}

/// Splits the validated scope set into its single user scope and its optional
/// workspace scope.
fn retrieval_scopes(scopes: &[Scope]) -> (Scope, Option<Scope>) {
    let mut user = Scope::default();
    let mut workspace = None;
    for scope in scopes {
        match scope.namespace.as_str() {
            NAMESPACE_USER => user = scope.clone(),
            NAMESPACE_WORKSPACE => workspace = Some(scope.clone()),
            _ => {}
        }
    }
    (user, workspace)
}

fn query_records(
    connection: &Connection,
    statement: &Statement,
    limit: usize,
) -> Result<Vec<Record>> {
    let mut prepared = connection
        .prepare(&statement.sql)
        .map_err(map_sqlite_error)?;
    let mut rows = prepared
        .query(params_from_iter(statement.arguments.iter()))
        .map_err(map_sqlite_error)?;
    let mut records = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        if records.len() >= limit {
            return Err(corrupt());
        }
        records.push(decode_record_row(row)?);
    }
    Ok(records)
}

/// Reads the FTS side of one row back and refuses a row whose index copy has
/// drifted from the record it points at.
fn check_fts_columns(row: &Row<'_>, record: &Record) -> Result<()> {
    let column = |index: usize| -> Result<String> {
        row.get::<_, Option<String>>(index)
            .map_err(map_sqlite_error)?
            .ok_or_else(corrupt)
    };
    let rank: Option<f64> = row.get(20).map_err(map_sqlite_error)?;
    let rank = rank.ok_or_else(corrupt)?;
    if column(15)? != record.id
        || column(16)? != record.text
        || column(17)? != record.kind
        || column(18)? != record.key
        || column(19)? != fts_labels(&record.labels)
        || !rank.is_finite()
    {
        return Err(corrupt());
    }
    Ok(())
}

fn query_ranked(connection: &Connection, statement: &Statement) -> Result<Vec<RetrievalCandidate>> {
    let mut prepared = connection
        .prepare(&statement.sql)
        .map_err(map_sqlite_error)?;
    let mut rows = prepared
        .query(params_from_iter(statement.arguments.iter()))
        .map_err(map_sqlite_error)?;
    let mut result: Vec<RetrievalCandidate> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        if result.len() >= MAX_RETRIEVAL_CANDIDATES {
            return Err(corrupt());
        }
        let record = decode_record_row(row)?;
        check_fts_columns(row, &record)?;
        if !seen.insert(record.id.clone()) {
            return Err(corrupt());
        }
        let lexical = result.len() + 1;
        result.push(make_candidate(record, false, lexical));
    }
    Ok(result)
}

/// Keeps the first candidate seen for each record ID, widening it with the
/// strongest `baseline` flag and the best lexical rank of its duplicates.
fn merge_candidates(
    left: Vec<RetrievalCandidate>,
    right: Vec<RetrievalCandidate>,
) -> Vec<RetrievalCandidate> {
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut result: Vec<RetrievalCandidate> = Vec::with_capacity(left.len() + right.len());
    for candidate in left.into_iter().chain(right) {
        if let Some(&index) = by_id.get(&candidate.record.id) {
            result[index].baseline = result[index].baseline || candidate.baseline;
            result[index].lexical = result[index].lexical.min(candidate.lexical);
            continue;
        }
        by_id.insert(candidate.record.id.clone(), result.len());
        result.push(candidate);
    }
    result
}

fn sort_candidates(candidates: &mut [RetrievalCandidate]) {
    candidates.sort_by(|left, right| {
        right
            .baseline
            .cmp(&left.baseline)
            .then_with(|| right.workspace.cmp(&left.workspace))
            .then_with(|| left.lexical.cmp(&right.lexical))
            .then_with(|| right.record.updated_at.cmp(&left.record.updated_at))
            .then_with(|| left.record.id.cmp(&right.record.id))
    });
}

fn candidate_keys(candidates: &[RetrievalCandidate]) -> Vec<RetrievalKey> {
    let mut seen = std::collections::BTreeSet::new();
    let mut keys = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if candidate.record.scope.namespace != NAMESPACE_USER || candidate.record.key.is_empty() {
            continue;
        }
        let key = RetrievalKey {
            kind: candidate.record.kind.clone(),
            key: candidate.record.key.clone(),
        };
        if seen.insert(key.clone()) {
            keys.push(key);
        }
    }
    keys
}

fn apply_workspace_replacements(
    candidates: Vec<RetrievalCandidate>,
    records: Vec<Record>,
) -> Vec<RetrievalCandidate> {
    let replacements: std::collections::BTreeMap<RetrievalKey, Record> = records
        .into_iter()
        .map(|record| {
            (
                RetrievalKey {
                    kind: record.kind.clone(),
                    key: record.key.clone(),
                },
                record,
            )
        })
        .collect();
    let mut result = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if candidate.record.scope.namespace != NAMESPACE_USER || candidate.record.key.is_empty() {
            result.push(candidate);
            continue;
        }
        let key = RetrievalKey {
            kind: candidate.record.kind.clone(),
            key: candidate.record.key.clone(),
        };
        match replacements.get(&key) {
            Some(replacement) => result.push(make_candidate(
                replacement.clone(),
                candidate.baseline,
                candidate.lexical,
            )),
            None => result.push(candidate),
        }
    }
    merge_candidates(Vec::new(), result)
}

fn has_all_labels(record: &Record, required: &[String]) -> bool {
    required.iter().all(|label| record.labels.contains(label))
}

/// Collapses records whose trimmed text is identical, keeping the strongest one
/// and carrying the baseline flag across the whole group.
fn dedupe_candidates(candidates: Vec<RetrievalCandidate>) -> Vec<RetrievalCandidate> {
    let mut by_digest: std::collections::HashMap<[u8; 32], usize> =
        std::collections::HashMap::new();
    let mut result: Vec<RetrievalCandidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let digest: [u8; 32] = Sha256::digest(candidate.record.text.trim().as_bytes()).into();
        match by_digest.get(&digest) {
            None => {
                by_digest.insert(digest, result.len());
                result.push(candidate);
            }
            Some(&index) => {
                let baseline = result[index].baseline || candidate.baseline;
                if better_dedupe_winner(&candidate, &result[index]) {
                    let mut winner = candidate;
                    winner.baseline = baseline;
                    result[index] = winner;
                } else {
                    result[index].baseline = baseline;
                }
            }
        }
    }
    result
}

fn better_dedupe_winner(left: &RetrievalCandidate, right: &RetrievalCandidate) -> bool {
    if left.workspace != right.workspace {
        return left.workspace;
    }
    if left.record.updated_at != right.record.updated_at {
        return left.record.updated_at > right.record.updated_at;
    }
    if left.lexical != right.lexical {
        return left.lexical < right.lexical;
    }
    left.record.id < right.record.id
}

/// The text a record's token cost is measured against, with sorted labels and
/// the record's own field order.
fn budget_text(record: &Record) -> String {
    let mut labels: Vec<&str> = record.labels.iter().map(String::as_str).collect();
    labels.sort_unstable();
    let mut out = String::from("{\"id\":");
    encode_string(&record.id, &mut out);
    out.push_str(",\"scope\":{\"Namespace\":");
    encode_string(&record.scope.namespace, &mut out);
    out.push_str(",\"ID\":");
    encode_string(&record.scope.id, &mut out);
    out.push_str("},\"kind\":");
    encode_string(&record.kind, &mut out);
    out.push_str(",\"key\":");
    encode_string(&record.key, &mut out);
    out.push_str(",\"labels\":");
    if labels.is_empty() {
        out.push_str("null");
    } else {
        out.push('[');
        for (index, label) in labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            encode_string(label, &mut out);
        }
        out.push(']');
    }
    out.push_str(",\"text\":");
    encode_string(&record.text, &mut out);
    out.push('}');
    out
}

fn estimate_tokens(estimator: Option<TokenEstimator>, text: &str) -> usize {
    if let Some(estimator) = estimator {
        return estimator(text).max(1);
    }
    if text.is_empty() {
        return 0;
    }
    1 + (text.len() - 1) / 3
}

impl Store {
    /// Ranks the records one recall should see, paging by an opaque cursor.
    pub fn retrieve(&self, request: &RetrievalRequest) -> Result<RetrievalResult> {
        validate_retrieval_request(request)?;
        let expression = build_fts_literal_expression(&request.query)?;
        let fingerprint = fingerprint_retrieval(request);
        let cursor = decode_retrieval_cursor(&request.cursor, &fingerprint)?;
        let start = cursor.as_ref().map_or(0, |cursor| cursor.ordinal);

        let (candidates, generation) = self.read_snapshot(request, &expression, cursor.as_ref())?;
        if start > candidates.len()
            || (start > 0
                && candidates[start - 1].record.id != cursor.expect("start implies a cursor").id)
        {
            return Err(invalid_cursor());
        }

        let mut result = RetrievalResult::default();
        let mut last_examined = start;
        for (index, candidate) in candidates.iter().enumerate().skip(start) {
            guard_record(self.guard(), &candidate.record)?;
            let estimate =
                estimate_tokens(request.estimate_tokens, &budget_text(&candidate.record));
            if estimate > request.token_budget {
                // An individually oversized record is examined and permanently
                // skipped rather than blocking the page behind it.
                last_examined = index + 1;
                continue;
            }
            if result.matches.len() >= request.limit
                || (!result.matches.is_empty()
                    && estimate > request.token_budget - result.used_tokens)
            {
                if last_examined == 0 {
                    return Err(corrupt());
                }
                result.next_cursor = encode_retrieval_cursor(
                    &fingerprint,
                    generation,
                    last_examined,
                    &candidates[last_examined - 1].record.id,
                )?;
                return Ok(result);
            }
            result.matches.push(RetrievalMatch {
                record: candidate.record.clone(),
                rank: index + 1,
            });
            result.used_tokens += estimate;
            last_examined = index + 1;
        }
        Ok(result)
    }

    fn read_snapshot(
        &self,
        request: &RetrievalRequest,
        expression: &str,
        cursor: Option<&super::cursor::RetrievalCursor>,
    ) -> Result<(Vec<RetrievalCandidate>, u64)> {
        let (user, workspace) = retrieval_scopes(&request.scopes);
        let (mut candidates, generation) = self.with_read(|connection| {
            in_read_transaction(connection, |connection| {
                let generation = read_generation(connection)?;
                if let Some(cursor) = cursor
                    && cursor.generation != generation
                {
                    return Err(Error::new(ErrorKind::Conflict));
                }
                let mut candidates: Vec<RetrievalCandidate> = Vec::new();
                if request.include_baseline {
                    let statement = build_baseline_query(request, &user);
                    for record in query_records(connection, &statement, MAX_BASELINE_RECORDS)? {
                        candidates.push(make_candidate(record, true, usize::MAX));
                    }
                }
                if !expression.is_empty() {
                    let statement = build_fts_candidate_query(request, expression);
                    let ranked = query_ranked(connection, &statement)?;
                    candidates = merge_candidates(candidates, ranked);
                }
                sort_candidates(&mut candidates);
                candidates.truncate(MAX_RETRIEVAL_CANDIDATES);
                if let Some(workspace) = &workspace {
                    let keys = candidate_keys(&candidates);
                    if !keys.is_empty() {
                        let statement =
                            build_workspace_replacement_query(request, workspace, &keys);
                        let replacements =
                            query_records(connection, &statement, MAX_RETRIEVAL_CANDIDATES)?;
                        candidates = apply_workspace_replacements(candidates, replacements);
                    }
                }
                Ok((candidates, generation))
            })
        })?;
        if !request.labels.is_empty() {
            candidates.retain(|candidate| has_all_labels(&candidate.record, &request.labels));
        }
        candidates = dedupe_candidates(candidates);
        sort_candidates(&mut candidates);
        candidates.truncate(MAX_RETRIEVAL_CANDIDATES);
        Ok((candidates, generation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::UpsertRequest;
    use crate::memory::sqlite::records::testsupport::{at, sample_record};
    use crate::memory::sqlite::testsupport::open_temp;

    fn request(store: &Store, query: &str) -> RetrievalRequest {
        RetrievalRequest {
            query: query.to_string(),
            scopes: vec![store.identity().expect("identity").user_scope],
            kinds: Vec::new(),
            labels: Vec::new(),
            include_expired: false,
            include_baseline: false,
            limit: 10,
            token_budget: 2000,
            cursor: String::new(),
            now: at(5),
            estimate_tokens: None,
        }
    }

    fn store_record(store: &Store, id: &str, scope: &Scope, key: &str, text: &str) -> Record {
        store
            .upsert(&UpsertRequest {
                record: sample_record(id, scope, key, text),
                expected_revision: None,
            })
            .expect("upsert")
    }

    fn ids(result: &RetrievalResult) -> Vec<String> {
        result
            .matches
            .iter()
            .map(|entry| entry.record.id.clone())
            .collect()
    }

    #[test]
    fn a_query_returns_only_the_records_it_matches() {
        let (_directory, store) = open_temp();
        let scope = store.identity().expect("identity").user_scope;
        store_record(&store, "rec-1", &scope, "tone", "prefers terse replies");
        store_record(&store, "rec-2", &scope, "editor", "uses neovim daily");

        let found = store
            .retrieve(&request(&store, "neovim"))
            .expect("retrieve");
        assert_eq!(ids(&found), vec!["rec-2"]);
        assert_eq!(found.matches[0].rank, 1);
        assert!(found.next_cursor.is_empty());
        assert!(found.used_tokens > 0);

        assert!(
            store
                .retrieve(&request(&store, "kubernetes"))
                .expect("retrieve")
                .matches
                .is_empty()
        );
    }

    #[test]
    fn the_baseline_set_leads_and_a_workspace_record_replaces_its_user_twin() {
        let (_directory, store) = open_temp();
        let user = store.identity().expect("identity").user_scope;
        let workspace = Scope::new(NAMESPACE_WORKSPACE, "work-otto");
        store_record(&store, "rec-1", &user, "tone", "prefers terse replies");
        store_record(
            &store,
            "rec-2",
            &workspace,
            "tone",
            "prefers terse replies here",
        );

        let mut input = request(&store, "terse");
        input.scopes = vec![user, workspace];
        input.include_baseline = true;
        let found = store.retrieve(&input).expect("retrieve");
        // The user record is replaced in place by the workspace record that
        // shares its (kind, key), so only one of the pair survives.
        assert_eq!(ids(&found), vec!["rec-2"]);
    }

    #[test]
    fn a_cursor_resumes_the_same_snapshot_and_a_write_invalidates_it() {
        let (_directory, store) = open_temp();
        let scope = store.identity().expect("identity").user_scope;
        for index in 1..=3 {
            store_record(
                &store,
                &format!("rec-{index}"),
                &scope,
                &format!("key-{index}"),
                &format!("prefers terse replies number {index}"),
            );
        }

        let mut input = request(&store, "terse");
        input.limit = 1;
        let first = store.retrieve(&input).expect("retrieve");
        assert_eq!(first.matches.len(), 1);
        assert!(!first.next_cursor.is_empty());

        let mut next = input.clone();
        next.cursor = first.next_cursor.clone();
        let second = store.retrieve(&next).expect("retrieve");
        assert_eq!(second.matches.len(), 1);
        assert_ne!(ids(&second), ids(&first));

        store_record(
            &store,
            "rec-4",
            &scope,
            "key-4",
            "prefers terse replies indeed",
        );
        assert_eq!(store.retrieve(&next).unwrap_err().kind, ErrorKind::Conflict);

        // A cursor minted for one query cannot be replayed against another.
        let mut other = input.clone();
        other.query = "replies".to_string();
        other.cursor = first.next_cursor;
        assert_eq!(
            store.retrieve(&other).unwrap_err().kind,
            ErrorKind::InvalidCursor
        );
    }

    #[test]
    fn an_oversized_record_is_skipped_instead_of_blocking_the_page() {
        let (_directory, store) = open_temp();
        let scope = store.identity().expect("identity").user_scope;
        store_record(&store, "rec-1", &scope, "tone", "terse ".repeat(200).trim());
        store_record(&store, "rec-2", &scope, "editor", "terse is best");

        let mut input = request(&store, "terse");
        input.token_budget = 120;
        let found = store.retrieve(&input).expect("retrieve");
        assert_eq!(ids(&found), vec!["rec-2"]);
    }

    #[test]
    fn the_default_estimator_uses_three_bytes_per_token() {
        assert_eq!(estimate_tokens(None, ""), 0);
        assert_eq!(estimate_tokens(None, "a"), 1);
        assert_eq!(estimate_tokens(None, "abc"), 1);
        assert_eq!(estimate_tokens(None, "abcd"), 2);
        // A custom estimator's non-positive answer is clamped up to one token.
        assert_eq!(estimate_tokens(Some(|_| 0), "abcd"), 1);
    }
}
