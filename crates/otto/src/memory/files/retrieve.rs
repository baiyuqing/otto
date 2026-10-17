//! Retrieval over the directory snapshot.
//!
//! One request reads a single snapshot: an optional standing baseline, the
//! records that match the query terms (every record when the query is empty),
//! and the workspace records that override user records with the same
//! `(kind, key)`. The result is ranked in memory, so the page cursor carries an
//! ordinal into that ranking and the id found there.

use sha2::{Digest, Sha256};

use super::cursor::{decode_retrieval_cursor, encode_retrieval_cursor, fingerprint_retrieval};
use super::records::standing_set;
use super::{FileStore, Snapshot};
use crate::memory::guard::guard_record;
use crate::memory::json::encode_string;
use crate::memory::validate::validate_retrieval_request;
use crate::memory::{
    Error, ErrorKind, MAX_FTS_TERM_BYTES, MAX_FTS_TERMS, MAX_RETRIEVAL_CANDIDATES,
    MAX_STANDING_TOKENS, NAMESPACE_USER, NAMESPACE_WORKSPACE, Record, Result, RetrievalMatch,
    RetrievalRequest, RetrievalResult, STANDING_ROW_GUARD, Scope, TokenEstimator, invalid_request,
};

/// One record in the ranked snapshot. `lexical` is `usize::MAX` minus the
/// number of query terms the record matches, so a smaller value ranks first;
/// it is `usize::MAX` for a record that matched no term.
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
        if left.baseline && right.baseline {
            return left
                .record
                .created_at
                .cmp(&right.record.created_at)
                .then_with(|| left.record.id.cmp(&right.record.id));
        }
        right
            .baseline
            .cmp(&left.baseline)
            .then_with(|| right.workspace.cmp(&left.workspace))
            .then_with(|| left.lexical.cmp(&right.lexical))
            .then_with(|| right.record.updated_at.cmp(&left.record.updated_at))
            .then_with(|| left.record.id.cmp(&right.record.id))
    });
}

/// Lowercased whitespace-separated query terms, without duplicates.
fn query_terms(query: &str) -> Result<Vec<String>> {
    let mut terms: Vec<String> = Vec::new();
    for term in query.split_whitespace() {
        if term.len() > MAX_FTS_TERM_BYTES {
            return Err(invalid_request("query term bounds"));
        }
        let term = term.to_lowercase();
        if !terms.contains(&term) {
            if terms.len() >= MAX_FTS_TERMS {
                return Err(invalid_request("query term count"));
            }
            terms.push(term);
        }
    }
    Ok(terms)
}

/// How many of `terms` occur as a case-insensitive substring of the record's
/// text, kind, key or any label.
fn matching_terms(record: &Record, terms: &[String]) -> usize {
    let fields: Vec<String> = [&record.text, &record.kind, &record.key]
        .into_iter()
        .chain(&record.labels)
        .map(|field| field.to_lowercase())
        .collect();
    terms
        .iter()
        .filter(|term| fields.iter().any(|field| field.contains(term.as_str())))
        .count()
}

fn passes_filters(record: &Record, request: &RetrievalRequest) -> bool {
    (request.kinds.is_empty() || request.kinds.contains(&record.kind))
        && (request.include_expired
            || record
                .expires_at
                .is_none_or(|expires| expires > request.now))
}

fn apply_workspace_replacements(
    candidates: Vec<RetrievalCandidate>,
    workspace: &Scope,
    snapshot: &Snapshot,
    request: &RetrievalRequest,
) -> Vec<RetrievalCandidate> {
    let mut result = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let replacement = if candidate.record.scope.namespace == NAMESPACE_USER
            && !candidate.record.key.is_empty()
        {
            snapshot.records.iter().find(|record| {
                &record.scope == workspace
                    && record.kind == candidate.record.kind
                    && record.key == candidate.record.key
                    && passes_filters(record, request)
            })
        } else {
            None
        };
        match replacement {
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

/// The token cost of one record against a recall budget.
pub(crate) fn record_cost(estimator: Option<TokenEstimator>, record: &Record) -> usize {
    estimate_tokens(estimator, &budget_text(record))
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

impl FileStore {
    /// Ranks the records one recall should see, paging by an opaque cursor.
    pub fn retrieve(&self, request: &RetrievalRequest) -> Result<RetrievalResult> {
        validate_retrieval_request(request)?;
        let terms = query_terms(&request.query)?;
        let fingerprint = fingerprint_retrieval(request);
        let cursor = decode_retrieval_cursor(&request.cursor, &fingerprint)?;
        let start = cursor.as_ref().map_or(0, |cursor| cursor.ordinal);

        let candidates = self.ranked(request, &terms)?;
        if start > candidates.len()
            || (start > 0
                && candidates[start - 1].record.id
                    != cursor.as_ref().expect("start implies a cursor").id)
        {
            return Err(invalid_cursor());
        }
        self.page(request, &fingerprint, &candidates, start)
    }

    fn page(
        &self,
        request: &RetrievalRequest,
        fingerprint: &str,
        candidates: &[RetrievalCandidate],
        start: usize,
    ) -> Result<RetrievalResult> {
        let mut result = RetrievalResult::default();
        let mut last_examined = start;
        // Standing records lead the snapshot and do not count against
        // `limit` or `token_budget`; they have their own budget, filled
        // oldest first, and the first one that does not fit closes it.
        let mut standing_used = 0;
        let mut standing_closed = false;
        let mut plain = 0;
        for (index, candidate) in candidates.iter().enumerate().skip(start) {
            guard_record(self.guard(), &candidate.record)?;
            let estimate = record_cost(request.estimate_tokens, &candidate.record);
            if candidate.baseline {
                last_examined = index + 1;
                if standing_closed || standing_used + estimate > MAX_STANDING_TOKENS {
                    standing_closed = true;
                    continue;
                }
                standing_used += estimate;
                result.matches.push(RetrievalMatch {
                    record: candidate.record.clone(),
                    rank: index + 1,
                    standing: true,
                });
                continue;
            }
            if estimate > request.token_budget {
                // An individually oversized record is examined and permanently
                // skipped rather than blocking the page behind it.
                last_examined = index + 1;
                continue;
            }
            if plain >= request.limit
                || (plain > 0 && estimate > request.token_budget - result.used_tokens)
            {
                if last_examined == 0 {
                    return Err(corrupt());
                }
                result.next_cursor = encode_retrieval_cursor(
                    fingerprint,
                    last_examined,
                    &candidates[last_examined - 1].record.id,
                )?;
                return Ok(result);
            }
            result.matches.push(RetrievalMatch {
                record: candidate.record.clone(),
                rank: index + 1,
                standing: false,
            });
            plain += 1;
            result.used_tokens += estimate;
            last_examined = index + 1;
        }
        Ok(result)
    }

    fn ranked(
        &self,
        request: &RetrievalRequest,
        terms: &[String],
    ) -> Result<Vec<RetrievalCandidate>> {
        let snapshot = self.snapshot()?;
        let (user, workspace) = retrieval_scopes(&request.scopes);
        let mut candidates: Vec<RetrievalCandidate> = Vec::new();
        if request.include_baseline {
            let standing = standing_set(
                &snapshot,
                &user,
                &request.kinds,
                request.include_expired,
                request.now,
            );
            for record in standing.into_iter().take(STANDING_ROW_GUARD) {
                candidates.push(make_candidate(record, true, usize::MAX));
            }
        }
        let mut matched = Vec::new();
        for record in &snapshot.records {
            if !request.scopes.contains(&record.scope) || !passes_filters(record, request) {
                continue;
            }
            let hits = matching_terms(record, terms);
            if terms.is_empty() {
                matched.push(make_candidate(record.clone(), false, usize::MAX));
            } else if hits > 0 {
                matched.push(make_candidate(record.clone(), false, usize::MAX - hits));
            }
        }
        candidates = merge_candidates(candidates, matched);
        sort_candidates(&mut candidates);
        candidates.truncate(MAX_RETRIEVAL_CANDIDATES);
        if let Some(workspace) = &workspace {
            candidates = apply_workspace_replacements(candidates, workspace, &snapshot, request);
        }
        if !request.labels.is_empty() {
            candidates.retain(|candidate| has_all_labels(&candidate.record, &request.labels));
        }
        candidates = dedupe_candidates(candidates);
        sort_candidates(&mut candidates);
        candidates.truncate(MAX_RETRIEVAL_CANDIDATES);
        Ok(candidates)
    }
}
