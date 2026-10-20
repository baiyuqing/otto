//! Tests for remember, forget, receipts and turn submission.

use super::*;

#[tokio::test]
async fn references_and_model_organization_mutations_fail_before_dispatch() {
    let (url, seen) = serve(|_| reply(200, json!({}))).await;
    let backend = client(service(&url));
    for organization in [None, Some("foreign".into())] {
        let mut target = reference("user");
        target.organization_id = organization;
        let request = InspectRequest {
            context: context(Origin::Model),
            query: InspectQuery::Get(target),
        };
        assert!(
            backend
                .inspect(&request, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    let mut remember = remember_request();
    remember.context.write_scope = Some(scope("organization"));
    assert!(
        backend
            .remember(&remember, &CancellationToken::new())
            .await
            .is_err()
    );
    let mut forget = forget_request();
    forget.target = ForgetTarget::Item(reference("organization"));
    assert!(
        backend
            .forget(&forget, &CancellationToken::new())
            .await
            .is_err()
    );
    let mut update = remember_request();
    let mut target = reference("workspace");
    target.revision = None;
    update.target = Some(target);
    assert!(
        backend
            .remember(&update, &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn remember_and_forget_receipts_validate_all_states_and_request_identity() {
    for state in [
        "applied",
        "no_change",
        "skipped",
        "failed",
        "pending",
        "unknown",
    ] {
        let (url, seen) = serve(move |_| {
            reply(
                if state == "pending" { 202 } else { 200 },
                receipt(state, "workspace", false),
            )
        })
        .await;
        let result = client(service(&url))
            .remember(&remember_request(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.items[0].reference.is_some(),
            matches!(state, "applied" | "no_change")
        );
        if matches!(state, "pending" | "unknown") {
            assert_ne!(result.completion, Completion::Complete);
        }
        let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
        assert_eq!(body["key"], "editor.default");
        assert!(body["context"].get("scopes").is_none());
        let (url, _) = serve(move |_| {
            reply(
                if state == "pending" { 202 } else { 200 },
                receipt(state, "user", true),
            )
        })
        .await;
        let result = client(service(&url))
            .forget(&forget_request(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.items[0].reference.is_some(),
            matches!(state, "applied" | "no_change")
        );
    }
    let valid = receipt("applied", "workspace", false);
    let mut cases = Vec::new();
    for (field, value) in [
        ("organization_id", json!("foreign")),
        ("revision", json!("")),
        ("id", json!("")),
    ] {
        let mut v = valid.clone();
        v["item"][field] = value;
        cases.push(v);
    }
    let mut v = valid.clone();
    v["request_id"] = json!("wrong");
    cases.push(v);
    let mut v = valid.clone();
    v["item"]["scope"] = json!(scope("user"));
    cases.push(v);
    let mut v = valid.clone();
    v["state"] = json!("skipped");
    cases.push(v);
    cases.push(json!({"state":"applied","id":"m-17"}));
    for body in cases {
        let (url, _) = serve(move |_| reply(200, body.clone())).await;
        assert_eq!(
            client(service(&url))
                .remember(&remember_request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::OutcomeUnknown
        );
    }
    for (status, state) in [(200, "pending"), (202, "applied"), (201, "applied")] {
        let (url, _) = serve(move |_| reply(status, receipt(state, "workspace", false))).await;
        assert_eq!(
            client(service(&url))
                .remember(&remember_request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::OutcomeUnknown
        );
    }
}
#[tokio::test]
async fn human_organization_write_preserves_full_context_and_service_acl() {
    let (url, seen) = serve(|_| reply(200, receipt("applied", "organization", false))).await;
    let mut request = remember_request();
    request.context.origin = Origin::Human;
    request.context.write_scope = Some(scope("organization"));
    assert_eq!(
        client(service(&url))
            .remember(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    assert!(seen.lock().unwrap().is_empty());
    let result = client(service(&url))
        .remember_organization_human(&request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.completion, Completion::Complete);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert_eq!(sent["origin"], "human");
    assert_eq!(sent["scope"], json!(scope("organization")));
    assert_eq!(sent["context"]["workspace_id"], "project-a");
    let (url, _) = serve(|_| {
        reply(
            403,
            json!({"error":{"code":"forbidden","message":"untrusted diagnostic"}}),
        )
    })
    .await;
    assert_eq!(
        client(service(&url))
            .remember_organization_human(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
}
#[tokio::test]
async fn errors_are_fixed_and_write_timeout_is_unknown() {
    for (status, code, expected) in [
        (400, "invalid_input", ErrorKind::InvalidInput),
        (401, "unauthorized", ErrorKind::Unauthorized),
        (403, "forbidden", ErrorKind::Unauthorized),
        (409, "conflict", ErrorKind::Conflict),
        (409, "idempotency_conflict", ErrorKind::Conflict),
        (413, "too_large", ErrorKind::InvalidInput),
        (429, "rate_limited", ErrorKind::RateLimited),
        (503, "unavailable", ErrorKind::Unavailable),
        (400, "unsupported_version", ErrorKind::Unsupported),
        (401, "bogus", ErrorKind::Unavailable),
    ] {
        let (url, _) = serve(move |_| {
            reply(
                status,
                json!({"error":{"code":code,"message":"do not expose"}}),
            )
        })
        .await;
        let error = service(&url)
            .recall(&recall_request(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, expected);
        assert!(!error.to_string().contains("do not expose"));
    }
    let (url, _) = serve(|_| {
        let mut r = reply(200, receipt("applied", "workspace", false));
        r.delay = Duration::from_millis(150);
        r
    })
    .await;
    let mut backend = service(&url);
    backend.write_timeout = Duration::from_millis(10);
    assert_eq!(
        backend
            .remember(&remember_request(), &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::OutcomeUnknown
    );
}
#[tokio::test]
async fn turns_require_valid_context_and_exact_202_empty_object() {
    let turn = wire::TurnRequest {
        context: RemoteService::context(&context(Origin::Model)).unwrap(),
        turn_id: "0123456789abcdef0123456789abcdef".into(),
        status: wire::TurnStatus::Success,
        messages: vec![],
    };
    for (status, body, ok) in [
        (202, json!({}), true),
        (200, json!({}), false),
        (202, json!({"ok":true}), false),
    ] {
        let (url, _) = serve(move |_| reply(status, body.clone())).await;
        assert_eq!(service(&url).upload_turn(&turn).await.is_ok(), ok);
    }
}
