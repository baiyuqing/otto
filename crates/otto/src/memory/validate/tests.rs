use super::*;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp")
}

fn active_record() -> Record {
    Record {
        id: "a1".into(),
        scope: Scope::new(NAMESPACE_USER, "install"),
        kind: "preference".into(),
        key: "editor".into(),
        text: "prefers tabs".into(),
        source: Provenance {
            origin: Some(Origin::Human),
            ..Provenance::default()
        },
        confidence: 1.0,
        revision: 1,
        created_at: now(),
        updated_at: now(),
        ..Record::default()
    }
}

#[test]
fn valid_name_accepts_only_the_name_alphabet() {
    assert!(valid_name("preference", MAX_KIND_BYTES));
    assert!(valid_name("a.b_c-1", MAX_KIND_BYTES));
    assert!(!valid_name("", MAX_KIND_BYTES));
    assert!(!valid_name("Preference", MAX_KIND_BYTES));
    assert!(!valid_name("1abc", MAX_KIND_BYTES));
    assert!(!valid_name("a b", MAX_KIND_BYTES));
}

#[test]
fn text_rejects_control_characters() {
    assert!(valid_text("plain text", 32));
    assert!(!valid_text("with\ttab", 32));
    assert!(!valid_text("with\u{80}c1", 32));
    assert!(!valid_text("toolong", 3));
}

#[test]
fn record_validation_accepts_a_well_formed_record() {
    validate_record(&active_record()).expect("valid record");
}

#[test]
fn record_validation_rejects_each_broken_field() {
    for (mutate, detail) in [
        (
            (|r: &mut Record| r.id = String::new()) as fn(&mut Record),
            "record ID exceeds 64 bytes",
        ),
        (|r| r.kind = "Bad".into(), "record kind exceeds 32 bytes"),
        (|r| r.text = String::new(), "record text exceeds 8192 bytes"),
        (|r| r.confidence = 2.0, "record confidence"),
        (|r| r.revision = 0, "record revision"),
        (|r| r.created_at = zero_time(), "record timestamps"),
    ] {
        let mut record = active_record();
        mutate(&mut record);
        let error = validate_record(&record).expect_err(detail);
        assert_eq!(error.kind, ErrorKind::InvalidRecord);
        assert_eq!(error.detail.as_deref(), Some(detail));
    }
}

#[test]
fn active_pending_origin_requires_a_decision() {
    let mut record = active_record();
    record.source.origin = Some(Origin::Model);
    let error = validate_record(&record).expect_err("undecided model record");
    assert_eq!(error.detail.as_deref(), Some("active source decision"));
}

#[test]
fn duplicate_labels_and_metadata_keys_are_rejected() {
    let mut record = active_record();
    record.labels = vec!["Tabs".into(), "tabs".into()];
    assert_eq!(
        validate_record(&record)
            .expect_err("labels")
            .detail
            .as_deref(),
        Some("duplicate label")
    );
}

#[test]
fn scope_sets_must_carry_exactly_one_user_scope() {
    let user = Scope::new(NAMESPACE_USER, "install");
    let workspace = Scope::new(NAMESPACE_WORKSPACE, "sha256:abc");
    validate_scopes(&[user.clone(), workspace.clone()], false, true).expect("phase one");
    assert!(validate_scopes(std::slice::from_ref(&workspace), false, true).is_err());
    assert!(validate_scopes(&[user.clone(), user], false, true).is_err());
    assert!(validate_scopes(&[], false, true).is_err());
}

#[test]
fn remember_requests_default_to_a_human_origin() {
    let request = RememberRequest {
        scope: Scope::new(NAMESPACE_USER, "install"),
        kind: "preference".into(),
        key: "editor".into(),
        text: "prefers tabs".into(),
        ..RememberRequest::default()
    };
    validate_remember_request(&request).expect("human default");

    let mut model = request.clone();
    model.source.origin = Some(Origin::Model);
    assert_eq!(
        validate_remember_request(&model)
            .expect_err("model")
            .detail
            .as_deref(),
        Some("source origin")
    );
}

#[test]
fn remember_update_requires_an_identity() {
    let mut request = RememberRequest {
        scope: Scope::new(NAMESPACE_USER, "install"),
        kind: "preference".into(),
        text: "prefers tabs".into(),
        expected_revision: Some(1),
        ..RememberRequest::default()
    };
    assert!(validate_remember_request(&request).is_err());
    request.key = "editor".into();
    validate_remember_request(&request).expect("keyed update");
}

#[test]
fn propose_requests_require_a_pending_origin() {
    let mut request = ProposeRequest {
        action: CandidateAction::Create,
        scope: Scope::new(NAMESPACE_USER, "install"),
        kind: "preference".into(),
        key: "editor".into(),
        text: "prefers tabs".into(),
        source: Provenance {
            origin: Some(Origin::Model),
            ..Provenance::default()
        },
        ..ProposeRequest::default()
    };
    validate_propose_request(&request).expect("model proposal");
    request.source.origin = Some(Origin::Human);
    assert_eq!(
        validate_propose_request(&request)
            .expect_err("human")
            .detail
            .as_deref(),
        Some("proposal source origin or decision")
    );
}

#[test]
fn a_user_scope_preference_or_instruction_create_needs_a_key() {
    let rule = Some("a user-scope preference or instruction needs a key");
    let propose = |namespace: &str, kind: &str| ProposeRequest {
        action: CandidateAction::Create,
        scope: Scope::new(namespace, "install"),
        kind: kind.into(),
        text: "prefers tabs".into(),
        source: Provenance {
            origin: Some(Origin::Model),
            ..Provenance::default()
        },
        ..ProposeRequest::default()
    };
    let remember = |namespace: &str, kind: &str| RememberRequest {
        scope: Scope::new(namespace, "install"),
        kind: kind.into(),
        text: "prefers tabs".into(),
        ..RememberRequest::default()
    };
    for kind in ["preference", "instruction"] {
        let error = validate_propose_request(&propose(NAMESPACE_USER, kind)).expect_err(kind);
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.detail.as_deref(), rule);
        let error = validate_remember_request(&remember(NAMESPACE_USER, kind)).expect_err(kind);
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.detail.as_deref(), rule);
    }
    validate_propose_request(&propose(NAMESPACE_USER, "fact")).expect("user fact");
    validate_remember_request(&remember(NAMESPACE_USER, "fact")).expect("user fact");
    validate_propose_request(&propose(NAMESPACE_WORKSPACE, "preference"))
        .expect("workspace preference");
    validate_remember_request(&remember(NAMESPACE_WORKSPACE, "preference"))
        .expect("workspace preference");
}

#[test]
fn forget_proposals_carry_no_content() {
    let mut request = ProposeRequest {
        action: CandidateAction::Forget,
        scope: Scope::new(NAMESPACE_USER, "install"),
        target_id: "a1".into(),
        base_revision: 1,
        source: Provenance {
            origin: Some(Origin::Model),
            ..Provenance::default()
        },
        ..ProposeRequest::default()
    };
    validate_propose_request(&request).expect("forget proposal");
    request.text = "leftover".into();
    assert_eq!(
        validate_propose_request(&request)
            .expect_err("content")
            .detail
            .as_deref(),
        Some("forget proposal content")
    );
}

#[test]
fn page_limits_and_budgets_are_bounded() {
    assert!(validate_page(0, "", MAX_PAGE_SIZE).is_err());
    assert!(validate_page(MAX_PAGE_SIZE + 1, "", MAX_PAGE_SIZE).is_err());
    validate_page(1, "", MAX_PAGE_SIZE).expect("valid page");
    assert!(validate_budget(1, MAX_TOKEN_BUDGET + 1, MAX_PAGE_SIZE).is_err());
    validate_budget(1, 100, MAX_PAGE_SIZE).expect("valid budget");
}
