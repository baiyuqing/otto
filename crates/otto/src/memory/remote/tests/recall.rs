//! Tests for recall requests, browsing and response filtering.

use super::*;

#[tokio::test]
async fn empty_query_browses_over_http_and_through_model_tool() {
    let (url, seen) = serve(|_| {
        reply(
            200,
            complete(vec![
                item("mandatory", "workspace", Some("editor"), true),
                item("organization", "organization", None, false),
                item("user", "user", None, false),
            ]),
        )
    })
    .await;
    let backend = Arc::new(client(service(&url)));
    let mut request = recall_request();
    request.query.clear();
    let result = backend
        .recall(&request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.items.len(), 3);
    assert!(result.items[0].remote.as_ref().unwrap().mandatory);
    assert_eq!(
        result.items[0].remote.as_ref().unwrap().view,
        MemoryView::Effective
    );
    let search = crate::tool::memory_service::tools(backend, context(Origin::Model), 8192)
        .into_iter()
        .find(|tool| tool.definition().name == "memory_search")
        .unwrap();
    let definition = search.definition();
    assert!(
        definition
            .description
            .contains("bounded set of effective memories")
    );
    let schema: Value =
        serde_json::from_str(definition.parameters.as_ref().unwrap().get()).unwrap();
    assert!(
        schema["properties"]["query"]["description"]
            .as_str()
            .unwrap()
            .contains("empty string")
    );
    let args = serde_json::value::RawValue::from_string(json!({"query":""}).to_string()).unwrap();
    let result = search.execute(&args, &CancellationToken::new()).await;
    assert!(!result.is_error, "{}", result.content);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (index, (path, headers, body)) in seen.iter().enumerate() {
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(path, "/v1/recall");
        assert!(headers.contains_key("authorization"));
        assert_eq!(body["query"], "");
        assert_eq!(body["context"]["organization_id"], "acme");
        assert_eq!(body["scope_filter"], Value::Null);
        assert_eq!(body["limit"], if index == 0 { 12 } else { 20 });
        assert_eq!(body["max_bytes"], if index == 0 { 8192 } else { 4000 });
    }
}

#[tokio::test]
async fn empty_query_keeps_request_bounds_and_whitespace_rejected_before_http() {
    let (url, seen) = serve(|_| reply(200, complete(vec![]))).await;
    let backend = Arc::new(client(service(&url)));
    let search = crate::tool::memory_service::tools(backend.clone(), context(Origin::Model), 8192)
        .into_iter()
        .find(|tool| tool.definition().name == "memory_search")
        .unwrap();
    for query in [" ", "\t", "\r\n", "\u{2003}"] {
        let mut request = recall_request();
        request.query = query.into();
        assert_eq!(
            backend
                .recall(&request, &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
        let args =
            serde_json::value::RawValue::from_string(json!({"query":query}).to_string()).unwrap();
        assert!(
            search
                .execute(&args, &CancellationToken::new())
                .await
                .is_error
        );
    }
    for case in 0..6 {
        let mut request = recall_request();
        request.query.clear();
        match case {
            0 => request.limit = 0,
            1 => request.limit = 65,
            2 => request.token_budget = 0,
            3 => request.token_budget = 8193,
            4 => request.scope_filter = Some(vec![]),
            _ => request.context.organization_id = None,
        }
        assert!(
            backend
                .recall(&request, &CancellationToken::new())
                .await
                .is_err(),
            "case {case}"
        );
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_query_keeps_authority_filter_mandatory_and_response_budgets() {
    let mut foreign = item("foreign", "user", None, false);
    foreign["scope"]["id"] = json!("other-user");
    let mut pinned = item("pinned", "user", None, false);
    pinned["pinned"] = json!(true);
    let cases = vec![
        (complete(vec![foreign]), None, 12, 8192),
        (complete(vec![pinned]), None, 12, 8192),
        (
            complete(vec![
                item("low", "user", None, false),
                item("high", "workspace", None, false),
            ]),
            None,
            12,
            8192,
        ),
        (
            complete(vec![
                item("high", "workspace", Some("same"), false),
                item("low", "user", Some("same"), false),
            ]),
            None,
            12,
            8192,
        ),
        (
            complete(vec![item("other", "workspace", None, false)]),
            Some(vec![ScopeNamespace::User]),
            12,
            8192,
        ),
        (
            complete(vec![
                item("one", "user", None, false),
                item("two", "user", None, false),
            ]),
            None,
            1,
            8192,
        ),
        (
            complete(vec![item("mandatory", "workspace", None, true)]),
            None,
            12,
            1,
        ),
    ];
    for (index, (body, filter, limit, budget)) in cases.into_iter().enumerate() {
        let (url, seen) = serve(move |_| reply(200, body.clone())).await;
        let mut request = recall_request();
        request.query.clear();
        request.scope_filter = filter;
        request.limit = limit;
        request.token_budget = budget;
        assert_eq!(
            client(service(&url))
                .recall(&request, &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable,
            "case {index}"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "case {index}");
    }
    let (url, seen) = serve(|_| {
        reply(
            200,
            complete(vec![item("winner", "organization", Some("editor"), true)]),
        )
    })
    .await;
    let mut request = recall_request();
    request.query.clear();
    request.scope_filter = Some(vec![ScopeNamespace::Organization]);
    assert_eq!(
        client(service(&url))
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert_eq!(body["scope_filter"], json!(["organization"]));
}

#[tokio::test]
async fn recall_sends_fixed_context_and_preserves_explicit_projection() {
    let (url, seen) = serve(|_| {
        reply(
            200,
            complete(vec![
                item("m-p", "workspace", Some("language"), true),
                item("m-o", "organization", Some("editor"), false),
                item("m-u", "user", None, false),
            ]),
        )
    })
    .await;
    let result = client(service(&url))
        .recall(&recall_request(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.items.len(), 3);
    let projection = result.items[0].remote.as_ref().unwrap();
    assert!(projection.mandatory && !projection.pinned);
    assert_eq!(projection.view, MemoryView::Effective);
    assert_eq!(
        result.items[0]
            .reference
            .as_ref()
            .unwrap()
            .organization_id
            .as_deref(),
        Some("acme")
    );
    let seen = seen.lock().unwrap();
    let (_, headers, body) = &seen[0];
    assert_eq!(headers["authorization"], "Bearer fixture-token");
    assert_eq!(headers["otto-memory-protocol"], "1.0");
    let body: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        body["context"],
        json!({"organization_id":"acme","user_id":"alice","workspace_id":"project-a","session_id":"s-1"})
    );
    assert!(body["scope_filter"].is_null());
    assert!(body["context"].get("scopes").is_none());
}
#[tokio::test]
async fn recall_discards_every_invalid_response_without_partial_results() {
    let valid = complete(vec![
        item("m-p", "workspace", Some("language"), true),
        item("m-o", "organization", Some("editor"), false),
    ]);
    let mut cases = Vec::new();
    for coverage in ["partial", "unknown"] {
        let mut v = valid.clone();
        v["coverage"] = json!(coverage);
        cases.push(v);
    }
    for field in ["organization_id", "user_id", "workspace_id", "session_id"] {
        let mut v = valid.clone();
        v["context"][field] = json!("foreign");
        cases.push(v);
    }
    for field in ["revision", "mandatory"] {
        let mut v = valid.clone();
        v["items"][0].as_object_mut().unwrap().remove(field);
        cases.push(v);
    }
    for (field, value) in [
        ("revision", json!("")),
        ("key", json!("Bad Key")),
        ("text", json!("")),
        ("id", json!("has space")),
        ("mandatory", json!("yes")),
    ] {
        let mut v = valid.clone();
        v["items"][0][field] = value;
        cases.push(v);
    }
    let mut v = valid.clone();
    v["items"][0]["scope"]["namespace"] = json!("global");
    cases.push(v);
    let mut v = valid.clone();
    v["items"][0]["scope"]["id"] = json!("foreign");
    cases.push(v);
    let mut v = valid.clone();
    v["resolution"] = json!("user_first");
    cases.push(v);
    cases.push(complete(vec![
        item("same", "user", None, false),
        item("same", "user", None, false),
    ]));
    cases.push(complete(vec![
        item("a", "workspace", Some("same"), false),
        item("b", "user", Some("same"), false),
    ]));
    cases.push(complete(vec![
        item("a", "user", None, false),
        item("b", "workspace", None, false),
    ]));
    cases.push(complete(vec![
        item("a", "workspace", None, false),
        item("b", "user", None, true),
    ]));
    let mut v = complete(vec![item("a", "user", None, false)]);
    v["items"][0]["pinned"] = json!(true);
    cases.push(v);
    for (i, body) in cases.into_iter().enumerate() {
        assert_eq!(recall_error(body).await, ErrorKind::Unavailable, "case {i}");
    }
    assert_eq!(
        recall_error(json!({"coverage":"complete","items":[]})).await,
        ErrorKind::Unavailable,
        "old v1 body"
    );
}
#[tokio::test]
async fn scoped_search_is_filtered_after_resolution_contract() {
    let (url, seen) = serve(|_| {
        reply(
            200,
            complete(vec![item("a", "organization", Some("language"), false)]),
        )
    })
    .await;
    let client = client(service(&url));
    let mut request = recall_request();
    request.scope_filter = Some(vec![ScopeNamespace::Organization]);
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert_eq!(body["scope_filter"], json!(["organization"]));
    request.scope_filter = Some(vec![ScopeNamespace::User]);
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unavailable
    );
    request.scope_filter = Some(vec![]);
    assert!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .is_err()
    );
}
