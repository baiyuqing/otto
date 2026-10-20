use super::*;

#[test]
fn error_display_appends_the_detail_after_the_category() {
    assert_eq!(
        Error::new(ErrorKind::Disabled).to_string(),
        "memory is disabled"
    );
    assert_eq!(
        invalid_request("installation ID").to_string(),
        "invalid memory request: installation ID"
    );
}

#[test]
fn opaque_ids_accept_only_the_id_alphabet() {
    assert!(valid_opaque_id("abc.DEF_09:-", MAX_ID_BYTES));
    assert!(!valid_opaque_id("", MAX_ID_BYTES));
    assert!(!valid_opaque_id("has space", MAX_ID_BYTES));
    assert!(!valid_opaque_id(
        &"a".repeat(MAX_ID_BYTES + 1),
        MAX_ID_BYTES
    ));
}

#[test]
fn the_default_policy_carries_the_documented_rules() {
    let base = PolicyRequest {
        origin: Some(Origin::Human),
        action: CandidateAction::Create,
        scope: Scope::new(NAMESPACE_USER, "install"),
        kind: "preference".into(),
        confidence: 1.0,
        source: Provenance::default(),
        valid: true,
        sensitive: false,
    };
    assert_eq!(decide_default_policy(&base), PolicyDecision::Accept);

    let mut migration = base.clone();
    migration.origin = Some(Origin::Migration);
    assert_eq!(decide_default_policy(&migration), PolicyDecision::Accept);

    for origin in [Origin::Model, Origin::Extractor, Origin::Import] {
        let mut pending = base.clone();
        pending.origin = Some(origin);
        assert_eq!(decide_default_policy(&pending), PolicyDecision::Pending);
    }

    let mut invalid = base.clone();
    invalid.valid = false;
    assert_eq!(decide_default_policy(&invalid), PolicyDecision::Reject);

    let mut sensitive_request = base.clone();
    sensitive_request.sensitive = true;
    assert_eq!(
        decide_default_policy(&sensitive_request),
        PolicyDecision::Reject
    );

    let mut mismatched = base.clone();
    mismatched.source.origin = Some(Origin::Model);
    assert_eq!(decide_default_policy(&mismatched), PolicyDecision::Reject);

    let mut unset = base;
    unset.origin = None;
    assert_eq!(decide_default_policy(&unset), PolicyDecision::Reject);
}

#[test]
fn new_id_is_thirty_two_hex_characters() {
    let id = new_id().expect("id");
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
}

#[test]
fn distinct_ids_retry_past_duplicates() {
    let counter = std::cell::Cell::new(0usize);
    let generate = || {
        let index = counter.get();
        counter.set(index + 1);
        Ok(format!("id{}", index / 2))
    };
    let ids = generate_distinct_ids(3, &generate).expect("ids");
    assert_eq!(ids, vec!["id0", "id1", "id2"]);
}
