//! Validation of list, search, remember, propose, forget and review requests.

use super::*;

pub(super) fn validate_scopes(scopes: &[Scope], allow_empty: bool, phase_one: bool) -> Result<()> {
    if scopes.is_empty() && !allow_empty {
        return Err(invalid_request("scope count"));
    }
    if scopes.len() > MAX_REQUEST_SCOPES {
        return Err(invalid_count(false, "scope count", MAX_REQUEST_SCOPES));
    }
    let mut seen = BTreeSet::new();
    let (mut users, mut workspaces) = (0usize, 0usize);
    for scope in scopes {
        validate_scope(scope)?;
        if !seen.insert(scope.clone()) {
            return Err(invalid_request("duplicate scope"));
        }
        if phase_one {
            match scope.namespace.as_str() {
                NAMESPACE_USER => users += 1,
                NAMESPACE_WORKSPACE => workspaces += 1,
                _ => return Err(invalid_request("phase 1 namespace")),
            }
        }
    }
    if phase_one && (users != 1 || workspaces > 1) {
        return Err(invalid_request("phase 1 scope set"));
    }
    Ok(())
}

pub(super) fn validate_filters(kinds: &[String], labels: &[String]) -> Result<()> {
    if kinds.len() > MAX_REQUEST_KINDS {
        return Err(invalid_count(false, "kind filter count", MAX_REQUEST_KINDS));
    }
    let mut seen = BTreeSet::new();
    for kind in kinds {
        if !valid_name(kind, MAX_KIND_BYTES) {
            return Err(invalid_request("kind filter"));
        }
        if !seen.insert(kind.as_str()) {
            return Err(invalid_request("duplicate kind filter"));
        }
    }
    if labels.len() > MAX_REQUEST_LABELS {
        return Err(invalid_count(
            false,
            "label filter count",
            MAX_REQUEST_LABELS,
        ));
    }
    let mut seen = BTreeSet::new();
    for label in labels {
        if !valid_semantic(label, MAX_LABEL_BYTES, true) {
            return Err(invalid_request_limit("label filter", MAX_LABEL_BYTES));
        }
        if !seen.insert(label.as_str()) {
            return Err(invalid_request("duplicate label filter"));
        }
    }
    Ok(())
}

pub(super) fn validate_page(limit: usize, cursor: &str, max: usize) -> Result<()> {
    if limit == 0 || limit > max {
        return Err(invalid_request_limit("page limit", max));
    }
    if !valid_cursor(cursor) {
        return Err(invalid_cursor());
    }
    Ok(())
}

pub(super) fn validate_now(now: DateTime<Utc>, include_expired: bool) -> Result<()> {
    if include_expired && now == zero_time() {
        return Ok(());
    }
    if !valid_timestamp(now) {
        return Err(invalid_request("request time"));
    }
    Ok(())
}

pub(crate) fn validate_list_request(request: &ListRequest) -> Result<()> {
    if request.all_scopes && !request.scopes.is_empty() {
        return Err(invalid_request("all scopes with explicit scopes"));
    }
    validate_scopes(&request.scopes, true, false)?;
    validate_filters(&request.kinds, &request.labels)?;
    validate_page(request.limit, &request.cursor, MAX_PAGE_SIZE)?;
    validate_list_states(&request.states)?;
    if request.unverified_only && request.verified_after.is_some() {
        return Err(invalid_request("unverified only with verified after"));
    }
    if !valid_optional_timestamp(request.updated_after)
        || !valid_optional_timestamp(request.verified_after)
    {
        return Err(invalid_request("list time filter"));
    }
    validate_now(request.now, request.include_expired)
}

fn validate_list_states(states: &[RecordState]) -> Result<()> {
    if states.len() > RecordState::ALL.len() {
        return Err(invalid_request("record state count"));
    }
    let mut seen = BTreeSet::new();
    for state in states {
        if !seen.insert(*state) {
            return Err(invalid_request("duplicate record state"));
        }
    }
    Ok(())
}

pub(crate) fn validate_candidate_list_request(request: &CandidateListRequest) -> Result<()> {
    validate_scopes(&request.scopes, true, false)?;
    if request.states.len() > MAX_REQUEST_KINDS {
        return Err(invalid_request("candidate state count"));
    }
    let mut seen = BTreeSet::new();
    for state in &request.states {
        if !seen.insert(*state) {
            return Err(invalid_request("duplicate candidate state"));
        }
    }
    validate_page(request.limit, &request.cursor, MAX_PAGE_SIZE)
}

pub(super) fn validate_query(value: &str, allow_empty: bool) -> Result<()> {
    if value.len() <= MAX_QUERY_BYTES && value.trim().is_empty() {
        if allow_empty {
            return Ok(());
        }
        return Err(invalid_request_limit("query", MAX_QUERY_BYTES));
    }
    if !valid_semantic(value, MAX_QUERY_BYTES, true) {
        return Err(invalid_request_limit("query", MAX_QUERY_BYTES));
    }
    Ok(())
}

pub(super) fn validate_budget(limit: usize, budget: usize, max_limit: usize) -> Result<()> {
    if limit == 0 || limit > max_limit {
        return Err(invalid_request_limit("result limit", max_limit));
    }
    if budget == 0 || budget > MAX_TOKEN_BUDGET {
        return Err(invalid_request_limit("token budget", MAX_TOKEN_BUDGET));
    }
    Ok(())
}

pub(crate) fn validate_retrieval_request(request: &RetrievalRequest) -> Result<()> {
    validate_query(&request.query, true)?;
    validate_scopes(&request.scopes, false, true)?;
    validate_filters(&request.kinds, &request.labels)?;
    validate_budget(request.limit, request.token_budget, MAX_PAGE_SIZE)?;
    validate_now(request.now, request.include_expired)
}

pub(crate) fn validate_search_request(request: &SearchRequest) -> Result<()> {
    validate_query(&request.query, true)?;
    validate_scopes(&request.scopes, false, true)?;
    validate_filters(&request.kinds, &request.labels)?;
    validate_budget(request.limit, request.token_budget, MAX_PAGE_SIZE)?;
    if !valid_cursor(&request.cursor) {
        return Err(invalid_cursor());
    }
    if !request.include_candidates && !request.candidate_states.is_empty() {
        return Err(invalid_request("candidate states require inclusion"));
    }
    let mut seen = BTreeSet::new();
    for state in &request.candidate_states {
        if !seen.insert(*state) {
            return Err(invalid_request("duplicate candidate state"));
        }
    }
    validate_now(request.now, request.include_expired)
}

/// A user-scope preference or instruction is recalled as a standing record
/// only when it has a key, so a create without one is rejected.
pub(super) fn require_standing_key(scope: &Scope, kind: &str, key: &str) -> Result<()> {
    if scope.namespace == NAMESPACE_USER
        && key.is_empty()
        && matches!(kind, "preference" | "instruction")
    {
        return Err(invalid_request(
            "a user-scope preference or instruction needs a key",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_transient_content(
    scope: &Scope,
    kind: &str,
    key: &str,
    text: &str,
    labels: &[String],
    metadata: &BTreeMap<String, String>,
    confidence: f64,
    expires: Option<DateTime<Utc>>,
    source: &Provenance,
    allowed_origin: fn(Option<Origin>) -> bool,
) -> Result<()> {
    validate_scope(scope)?;
    if !valid_name(kind, MAX_KIND_BYTES)
        || !valid_semantic(key, MAX_SEMANTIC_KEY_BYTES, false)
        || !valid_semantic(text, MAX_RECORD_TEXT_BYTES, true)
    {
        return Err(invalid_request("content fields"));
    }
    validate_labels(labels, false)?;
    validate_metadata(metadata, false)?;
    if !valid_confidence(confidence) {
        return Err(invalid_request("confidence"));
    }
    if !valid_optional_timestamp(expires) {
        return Err(invalid_request("expiry"));
    }
    if !allowed_origin(source.origin) {
        return Err(invalid_request("source origin"));
    }
    validate_provenance(source, false, false)
}

pub(crate) fn validate_remember_request(request: &RememberRequest) -> Result<()> {
    let mut source = request.source.clone();
    if provenance_zero(&source) {
        source.origin = Some(Origin::Human);
    }
    validate_transient_content(
        &request.scope,
        &request.kind,
        &request.key,
        &request.text,
        &request.labels,
        &request.metadata,
        request.confidence,
        request.expires_at,
        &source,
        decision_origin,
    )?;
    if source.decision_at.is_some() {
        return Err(invalid_request("manager source decision"));
    }
    match request.expected_revision {
        None => {
            if !request.id.is_empty() {
                return Err(invalid_request("create ID"));
            }
            require_standing_key(&request.scope, &request.kind, &request.key)?;
        }
        Some(expected) => {
            let identified = !request.id.is_empty() || !request.key.is_empty();
            let id_ok = request.id.is_empty() || valid_opaque_id(&request.id, MAX_ID_BYTES);
            if !valid_revision(expected) || !identified || !id_ok {
                return Err(invalid_request("update identity and revision"));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_propose_request(request: &ProposeRequest) -> Result<()> {
    if !valid_semantic(&request.reason, MAX_REASON_BYTES, false) {
        return Err(invalid_request_limit("proposal reason", MAX_REASON_BYTES));
    }
    if !pending_origin(request.source.origin)
        || request.source.decision_at.is_some()
        || request.source.decision_source.is_some()
    {
        return Err(invalid_request("proposal source origin or decision"));
    }
    if !request.action.carries_content() {
        // Forget, retract and needs_review name a target and carry no content.
        validate_scope(&request.scope)?;
        if !valid_opaque_id(&request.target_id, MAX_ID_BYTES)
            || !valid_revision(request.base_revision)
        {
            return Err(invalid_request("proposal target without content"));
        }
        let route_only = presentation_route_only(&request.metadata);
        if !request.kind.is_empty()
            || !request.key.is_empty()
            || !request.text.is_empty()
            || !request.labels.is_empty()
            || !route_only
            || request.confidence != 0.0
            || request.expires_at.is_some()
        {
            return Err(invalid_request(
                "proposal content of an action without content",
            ));
        }
        return validate_provenance(&request.source, false, false);
    }
    validate_transient_content(
        &request.scope,
        &request.kind,
        &request.key,
        &request.text,
        &request.labels,
        &request.metadata,
        request.confidence,
        request.expires_at,
        &request.source,
        pending_origin,
    )?;
    match request.action {
        CandidateAction::Create => {
            if !request.target_id.is_empty() || request.base_revision != 0 {
                return Err(invalid_request("create proposal target"));
            }
            require_standing_key(&request.scope, &request.kind, &request.key)?;
        }
        CandidateAction::Update | CandidateAction::Supersede | CandidateAction::Correct => {
            if !valid_opaque_id(&request.target_id, MAX_ID_BYTES)
                || !valid_revision(request.base_revision)
            {
                return Err(invalid_request("update proposal target"));
            }
        }
        CandidateAction::Forget | CandidateAction::Retract | CandidateAction::NeedsReview => {
            unreachable!("handled above")
        }
    }
    Ok(())
}

pub(crate) fn validate_forget_request(request: &ForgetRequest) -> Result<()> {
    validate_record_ref(&request.reference)?;
    if !valid_revision(request.expected_revision) {
        return Err(invalid_request("expected revision"));
    }
    if request.purge_backups && !request.confirm_purge {
        return Err(invalid_request("purge confirmation"));
    }
    Ok(())
}

pub(crate) fn validate_record_ref(reference: &RecordRef) -> Result<()> {
    validate_scope(&reference.scope)?;
    if !valid_opaque_id(&reference.id, MAX_ID_BYTES) {
        return Err(invalid_request("record ID"));
    }
    Ok(())
}

/// A state change names the current revision; `Superseded` needs a
/// replacement id other than the record itself, every other state none.
pub(crate) fn validate_set_state_request(request: &SetStateRequest) -> Result<()> {
    validate_record_ref(&request.reference)?;
    if !valid_revision(request.expected_revision) {
        return Err(invalid_request("expected revision"));
    }
    match request.state {
        RecordState::Superseded => {
            if !valid_opaque_id(&request.superseded_by, MAX_ID_BYTES)
                || request.superseded_by == request.reference.id
            {
                return Err(invalid_request("replacement record ID"));
            }
        }
        _ => {
            if !request.superseded_by.is_empty() {
                return Err(invalid_request(
                    "replacement record ID is only valid for superseded",
                ));
            }
        }
    }
    if !request.reason.is_empty() && !valid_semantic(&request.reason, MAX_STATE_REASON_BYTES, true)
    {
        return Err(invalid_request("state reason"));
    }
    Ok(())
}

/// A verification names the current revision and an optional next review
/// deadline.
pub(crate) fn validate_verify_request(request: &VerifyRequest) -> Result<()> {
    validate_record_ref(&request.reference)?;
    if !valid_revision(request.expected_revision) {
        return Err(invalid_request("expected revision"));
    }
    if !valid_optional_timestamp(request.review_at) {
        return Err(invalid_request("review time"));
    }
    Ok(())
}

/// A restore names an earlier revision and the current one the caller read;
/// both are positive, and the restored revision precedes the current one.
pub(crate) fn validate_restore_request(request: &RestoreRequest) -> Result<()> {
    validate_record_ref(&request.reference)?;
    if !valid_revision(request.revision) || !valid_revision(request.expected_revision) {
        return Err(invalid_request("restore revision"));
    }
    if request.revision >= request.expected_revision {
        return Err(invalid_request(
            "restore revision must precede the current revision",
        ));
    }
    Ok(())
}

pub(crate) fn validate_candidate_ref(reference: &CandidateRef) -> Result<()> {
    validate_scope(&reference.scope)?;
    if !valid_opaque_id(&reference.id, MAX_ID_BYTES) {
        return Err(invalid_request("candidate ID"));
    }
    Ok(())
}

pub(crate) fn validate_record_key(key: &RecordKey) -> Result<()> {
    validate_scope(&key.scope)?;
    if !valid_name(&key.kind, MAX_KIND_BYTES)
        || !valid_semantic(&key.key, MAX_SEMANTIC_KEY_BYTES, true)
    {
        return Err(invalid_request("record key"));
    }
    Ok(())
}

pub(crate) fn validate_upsert_request(request: &UpsertRequest) -> Result<()> {
    match request.expected_revision {
        None => {
            if request.record.revision != 0 {
                return Err(invalid_request("create revision"));
            }
            let mut record = request.record.clone();
            record.revision = 1;
            validate_record(&record)
        }
        Some(expected) => {
            if !valid_revision(expected) || request.record.revision != expected {
                return Err(invalid_request("expected revision"));
            }
            validate_record(&request.record)
        }
    }
}

pub(crate) fn validate_store_forget_request(request: &StoreForgetRequest) -> Result<()> {
    validate_record_ref(&request.reference)?;
    if !valid_revision(request.expected_revision) || !valid_timestamp(request.forgotten_at) {
        return Err(invalid_request("forget revision or time"));
    }
    Ok(())
}

pub(crate) fn validate_proposal_batch(candidates: &[Candidate]) -> Result<()> {
    if candidates.is_empty() {
        return Err(invalid_request("candidate batch count"));
    }
    if candidates.len() > MAX_CANDIDATE_BATCH {
        return Err(invalid_count(
            false,
            "candidate batch count",
            MAX_CANDIDATE_BATCH,
        ));
    }
    for candidate in candidates {
        validate_candidate(candidate)?;
        if candidate.state != CandidateState::Pending {
            return Err(invalid_request("candidate batch state"));
        }
    }
    Ok(())
}

pub(crate) fn validate_store_review_request(
    request: &StoreReviewRequest,
    candidate: &Candidate,
) -> Result<()> {
    validate_candidate_ref(&request.reference)?;
    validate_candidate(candidate)?;
    if candidate.state != CandidateState::Pending {
        return Err(invalid_request("review candidate state"));
    }
    if request.reference.scope != candidate.proposed.scope || request.reference.id != candidate.id {
        return Err(invalid_request("review candidate reference"));
    }
    if !decision_origin(request.decision_source) || !valid_timestamp(request.decided_at) {
        return Err(invalid_request("review decision"));
    }
    if !request.result_record_id.is_empty()
        && !valid_opaque_id(&request.result_record_id, MAX_ID_BYTES)
    {
        return Err(invalid_request("review result ID"));
    }
    if request
        .target_revision
        .is_some_and(|revision| !valid_revision(revision))
    {
        return Err(invalid_request("review target revision"));
    }
    if request.decided_at < candidate.created_at {
        return Err(invalid_request("review decision time"));
    }
    if request.decision == ReviewDecision::Reject {
        if request.edited.is_some()
            || !request.result_record_id.is_empty()
            || request.target_revision.is_some()
        {
            return Err(invalid_request("rejected review fields"));
        }
        return Ok(());
    }
    if !candidate.action.carries_content() && request.edited.is_some() {
        return Err(invalid_request(
            "review edit of a candidate without content",
        ));
    }
    if candidate.action == CandidateAction::Create {
        if !valid_opaque_id(&request.result_record_id, MAX_ID_BYTES)
            || request.target_revision.is_some()
        {
            return Err(invalid_request("accepted create fields"));
        }
    } else {
        // A supersede or correct writes a replacement record and so needs its ID.
        let writes_replacement = action_writes_new_record(candidate.action);
        if writes_replacement != !request.result_record_id.is_empty() {
            return Err(invalid_request("accepted target result ID"));
        }
        if writes_replacement && !valid_opaque_id(&request.result_record_id, MAX_ID_BYTES) {
            return Err(invalid_request("accepted replacement result ID"));
        }
        let target = request.target_revision.unwrap_or(candidate.base_revision);
        if candidate.action == CandidateAction::Update
            && target != candidate.base_revision
            && request.edited.is_none()
        {
            return Err(invalid_request("review rebase edit"));
        }
    }
    if let Some(edited) = &request.edited {
        if !candidate.action.carries_content() || edited.scope != candidate.proposed.scope {
            return Err(invalid_request("review edit scope"));
        }
        validate_proposed_record(edited, candidate.action)?;
    }
    Ok(())
}

pub(crate) fn validate_tombstone(tombstone: &Tombstone) -> Result<()> {
    if !valid_opaque_id(&tombstone.id, MAX_ID_BYTES) {
        return Err(invalid_record("tombstone ID"));
    }
    validate_scope(&tombstone.scope).map_err(|_| invalid_record("tombstone scope"))?;
    if !valid_revision(tombstone.revision)
        || !valid_timestamp(tombstone.created_at)
        || !valid_timestamp(tombstone.updated_at)
        || !valid_timestamp(tombstone.forgotten_at)
    {
        return Err(invalid_record("tombstone revision or time"));
    }
    if tombstone.updated_at < tombstone.created_at
        || tombstone.forgotten_at < tombstone.created_at
        || tombstone.forgotten_at > tombstone.updated_at
    {
        return Err(invalid_record("tombstone time relationship"));
    }
    Ok(())
}

pub(crate) fn validate_recall_request(request: &RecallRequest) -> Result<()> {
    validate_query(&request.query, false)?;
    validate_filters(&request.kinds, &[])?;
    validate_budget(request.limit, request.token_budget, MAX_RECALL_RECORDS)
}

pub(crate) fn validate_bind_scopes(scopes: &[Scope], default_write_scope: &Scope) -> Result<()> {
    validate_scopes(scopes, false, true)?;
    validate_scope(default_write_scope)?;
    if scopes.contains(default_write_scope) {
        return Ok(());
    }
    Err(invalid_request("default write scope"))
}
