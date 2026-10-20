//! Tests for response rendering, status protocol and raw reads.

use super::*;

#[tokio::test]
async fn golden_rendering_and_exact_escaped_byte_budget() {
    let fixture = include_str!("../fixtures/v1/recall_response.json");
    let expected = include_str!("../fixtures/v1/recall_rendered.txt");
    let response: wire::RecallResponse = wire::decode(fixture.as_bytes()).unwrap();
    let records: Vec<_> = response
        .items
        .into_iter()
        .map(|r| {
            record(
                &RemoteService::item(
                    r.item,
                    &context(Origin::Model),
                    r.mandatory,
                    MemoryView::Effective,
                )
                .unwrap(),
            )
        })
        .collect();
    assert_eq!(render_memory_context(&records), expected);
    let (url, _) = serve(move |_| Reply {
        status: 200,
        protocol: Some("1.0"),
        body: fixture.as_bytes().to_vec(),
        delay: Duration::ZERO,
    })
    .await;
    let client = client(service(&url));
    let mut request = recall_request();
    request.token_budget = expected.len();
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap()
            .used_tokens,
        Some(expected.len())
    );
    request.token_budget -= 1;
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unavailable
    );
    assert_eq!(render_memory_context(&[]), "");
}
#[tokio::test]
async fn strict_status_protocol_duplicates_size_timeout_and_cancellation() {
    for status in [201, 202, 204, 206] {
        let (url, _) = serve(move |_| reply(status, complete(vec![]))).await;
        assert_eq!(
            client(service(&url))
                .recall(&recall_request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
    }
    for protocol in [None, Some("2.0"), Some("1."), Some("x1.0")] {
        let (url, _) = serve(move |_| {
            let mut r = reply(200, complete(vec![]));
            r.protocol = protocol;
            r
        })
        .await;
        assert!(
            client(service(&url))
                .recall(&recall_request(), &CancellationToken::new())
                .await
                .is_err()
        );
    }
    let (url, _) = serve(|_| {
        let mut r = reply(200, complete(vec![]));
        r.protocol = Some("1.23");
        r
    })
    .await;
    assert!(
        client(service(&url))
            .recall(&recall_request(), &CancellationToken::new())
            .await
            .is_ok()
    );
    for body in [
        b"{\"coverage\":\"complete\",\"coverage\":\"partial\"}".to_vec(),
        vec![b' '; MAX_RESPONSE_BYTES + 1],
    ] {
        let (url, _) = serve(move |_| Reply {
            status: 200,
            protocol: Some("1.0"),
            body: body.clone(),
            delay: Duration::ZERO,
        })
        .await;
        assert!(
            client(service(&url))
                .recall(&recall_request(), &CancellationToken::new())
                .await
                .is_err()
        );
    }
    let (url, _) = serve(|_| {
        let mut r = reply(200, complete(vec![]));
        r.delay = Duration::from_millis(150);
        r
    })
    .await;
    let mut backend = service(&url);
    backend.recall_timeout = Duration::from_millis(10);
    assert_eq!(
        backend
            .recall(&recall_request(), &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unavailable
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        service(&url)
            .recall(&recall_request(), &cancel)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Canceled
    );
}
#[tokio::test]
async fn raw_get_checks_tenant_scope_and_status_and_never_returns_effective_view() {
    let (url,seen)=serve(|_|reply(200,json!({"view":"raw","organization_id":"acme","item":{"id":"m-17","scope":scope("user"),"revision":"r4","key":"editor","text":"shadowed","pinned":false}}))).await;
    let client = client(service(&url));
    let request = InspectRequest {
        context: context(Origin::Human),
        query: InspectQuery::Get(reference("user")),
    };
    let result = client
        .inspect(&request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        result.items[0].remote.as_ref().unwrap().view,
        MemoryView::Raw
    );
    assert_eq!(result.state, "raw");
    assert_eq!(seen.lock().unwrap().len(), 1);
    for body in [
        json!({"view":"effective","organization_id":"acme","item":{}}),
        json!({"view":"raw","organization_id":"other","item":{}}),
    ] {
        let (url, _) = serve(move |_| reply(200, body.clone())).await;
        assert!(
            service(&url)
                .inspect(&request, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    for (status, body, ok) in [
        (404, json!({}), true),
        (404, json!({"error":"not found"}), false),
        (201, json!({}), false),
    ] {
        let (url, _) = serve(move |_| reply(status, body.clone())).await;
        assert_eq!(
            service(&url)
                .inspect(&request, &CancellationToken::new())
                .await
                .is_ok(),
            ok
        );
    }
}

#[tokio::test]
async fn exact_search_with_only_id_uses_host_scopes() {
    let (url, seen) = serve_request(|path, body| {
        assert_eq!(path, "/v1/get");
        let request: Value = serde_json::from_slice(body).unwrap();
        if request["scope"]["namespace"] == "workspace" {
            reply(200, json!({"view":"raw","organization_id":"acme","item":{"id":"m-17","scope":scope("workspace"),"revision":"r4","key":"editor","text":"workspace value","pinned":false}}))
        } else {
            reply(404, json!({}))
        }
    }).await;
    let search = crate::tool::memory_service::tools(
        Arc::new(client(service(&url))),
        context(Origin::Model),
        8192,
    )
    .into_iter()
    .find(|tool| tool.definition().name == "memory_search")
    .unwrap();
    let args = serde_json::value::RawValue::from_string(json!({"id":"m-17"}).to_string()).unwrap();
    let result = search.execute(&args, &CancellationToken::new()).await;
    assert!(
        !result.is_error,
        "{}; requests={}",
        result.content,
        seen.lock().unwrap().len()
    );
    assert!(result.content.contains("workspace value"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|(_, _, body)| {
        let request: Value = serde_json::from_slice(body).unwrap();
        request["context"]["organization_id"] == "acme" && request["id"] == "m-17"
    }));
}
