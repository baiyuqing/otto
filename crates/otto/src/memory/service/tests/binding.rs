//! Tests for turn-text normalization, closing and the null service.

use super::*;

/// Turn text is raw user input: multi-line, untrimmed and unbounded,
/// while the store's query is single-line, trimmed and bounded. The
/// binding normalizes instead of failing the recall.
#[tokio::test]
async fn a_binding_normalizes_raw_turn_text_into_a_query() {
    let (_directory, service) = service();
    let scope = user_scope();
    let created = service
        .remember(&remember(&scope, "editor", "prefers vim for editing code"))
        .expect("create");
    let binding = service
        .bind(BindOptions {
            scopes: vec![scope.clone()],
            default_write_scope: scope,
            ..BindOptions::default()
        })
        .expect("bind");

    let query = format!("  vim\n{}\n", "padding ".repeat(2000));
    let result = binding
        .recall(&RecallRequest {
            query,
            limit: 10,
            token_budget: 1000,
            ..RecallRequest::default()
        })
        .expect("recall");
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].id, created.id);
}

#[test]
fn closing_the_service_invalidates_it_and_every_binding() {
    let (_directory, service) = service();
    let scope = user_scope();
    let binding = service
        .bind(BindOptions {
            scopes: vec![scope.clone()],
            default_write_scope: scope.clone(),
            ..BindOptions::default()
        })
        .expect("bind");

    service.close().expect("close");
    service.close().expect("a second close is a no-op");

    assert!(
        service
            .remember(&remember(&scope, "editor", "vim"))
            .expect_err("closed")
            .is(ErrorKind::Closed)
    );
    assert!(
        binding
            .recall(&RecallRequest::default())
            .expect_err("closed")
            .is(ErrorKind::Closed)
    );
    assert!(
        service
            .bind(BindOptions {
                scopes: vec![scope.clone()],
                default_write_scope: scope,
                ..BindOptions::default()
            })
            .expect_err("closed")
            .is(ErrorKind::Closed)
    );
}

/// A null service answers with a safe category, but its binding still
/// recalls nothing rather than failing.
#[test]
fn a_null_service_answers_with_its_category_and_recalls_nothing() {
    let scope = user_scope();
    for (reason, want) in [
        (None, ErrorKind::Disabled),
        (Some(ErrorKind::Disabled), ErrorKind::Disabled),
        (Some(ErrorKind::Unavailable), ErrorKind::Unavailable),
        (Some(ErrorKind::Corrupt), ErrorKind::Unavailable),
    ] {
        let service = Arc::new(Service::null(reason));
        assert!(service.is_null());
        let error = service
            .get(&RecordRef {
                scope: scope.clone(),
                id: "abc".into(),
            })
            .expect_err("null");
        assert!(error.is(want), "reason {reason:?} gave {error}");
        assert!(
            service
                .remember(&remember(&scope, "editor", "vim"))
                .expect_err("null")
                .is(want)
        );

        let binding = service
            .bind(BindOptions {
                scopes: vec![scope.clone()],
                default_write_scope: scope.clone(),
                ..BindOptions::default()
            })
            .expect("a null service still binds");
        assert_eq!(
            binding
                .recall(&RecallRequest::default())
                .expect("null recall")
                .records
                .len(),
            0
        );

        service.close().expect("close");
        assert!(
            service
                .get(&RecordRef {
                    scope: scope.clone(),
                    id: "abc".into()
                })
                .expect_err("closed")
                .is(ErrorKind::Closed)
        );
        assert!(
            binding
                .recall(&RecallRequest::default())
                .expect_err("closed")
                .is(ErrorKind::Closed)
        );
    }
}

/// Binding still validates its scopes, on the null service as on a real
/// one.
#[test]
fn binding_rejects_a_write_scope_it_does_not_read() {
    let service = Arc::new(Service::null(None));
    let error = service
        .bind(BindOptions {
            scopes: vec![user_scope()],
            default_write_scope: Scope::new(NAMESPACE_WORKSPACE, "workspace-1"),
            ..BindOptions::default()
        })
        .expect_err("the write scope is not readable");
    assert!(error.is(ErrorKind::InvalidRequest), "error = {error}");
}
