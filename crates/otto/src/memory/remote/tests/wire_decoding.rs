//! Tests for the v1 wire format, constructor limits and model tools.

use super::*;

#[test]
fn wire_rejects_nested_duplicate_unknown_fields_and_unknown_enums() {
    assert!(wire::decode::<Value>(br#"{"extra":{"same":1,"same":2}}"#).is_err());
    assert!(wire::decode::<Value>(br#"[{"same":1,"same":2}]"#).is_err());
    assert!(wire::decode::<wire::Scope>(br#"{"namespace":"global","id":"a"}"#).is_err());
    assert!(wire::decode::<Value>(br#"{"extra":[null,true,3,1.5,"x"]}"#).is_ok());
}
#[test]
fn constructor_rejects_non_loopback_http_and_redirecting_urls_without_network() {
    for url in [
        "http://example.com",
        "https://u:p@example.com",
        "https://example.com/v1",
        "https://example.com?q=1",
        "https://example.com#fragment",
    ] {
        assert!(
            RemoteService::new(Config {
                url: url.into(),
                token: "fixture-token".into(),
                recall_timeout: Duration::from_secs(3),
                logger: Arc::new(Logger::discard())
            })
            .is_err(),
            "{url}"
        );
    }
}

#[test]
fn every_v1_fixture_round_trips_and_missing_nullable_item_key_is_invalid() {
    fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(fixture: &str) {
        let value: T = wire::decode(fixture.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::from_str::<Value>(fixture).unwrap()
        );
    }
    round_trip::<wire::RecallBody>(include_str!("../fixtures/v1/recall_request.json"));
    round_trip::<wire::RecallResponse>(include_str!("../fixtures/v1/recall_response.json"));
    round_trip::<wire::GetBody>(include_str!("../fixtures/v1/get_request.json"));
    round_trip::<wire::GetResponse>(include_str!("../fixtures/v1/get_response.json"));
    round_trip::<wire::RememberBody>(include_str!("../fixtures/v1/remember_request.json"));
    round_trip::<wire::ForgetBody>(include_str!("../fixtures/v1/forget_request.json"));
    round_trip::<wire::Receipt>(include_str!("../fixtures/v1/receipt.json"));
    round_trip::<wire::TurnRequest>(include_str!("../fixtures/v1/turns_request.json"));
    round_trip::<wire::ErrorBody>(include_str!("../fixtures/v1/error.json"));
    let mut recalled = item("a", "user", None, false);
    recalled.as_object_mut().unwrap().remove("key");
    assert!(wire::decode::<wire::RecalledItem>(&serde_json::to_vec(&recalled).unwrap()).is_err());
}
#[tokio::test]
async fn no_project_context_has_only_organization_and_user_and_defaults_to_user() {
    let (url, seen) = serve(|_| reply(200, receipt("applied", "user", false))).await;
    let mut request = remember_request();
    request.context.workspace_id = None;
    request
        .context
        .scopes
        .retain(|scope| scope.namespace != "workspace");
    request.context.write_scope = Some(scope("user"));
    client(service(&url))
        .remember(&request, &CancellationToken::new())
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert!(body["context"]["workspace_id"].is_null());
    assert_eq!(body["scope"], json!(scope("user")));
}
#[tokio::test]
async fn model_tools_bind_references_and_nullable_key_without_tenant_selection() {
    use crate::tool::memory_service::tools;
    use serde_json::value::RawValue;
    let (url, seen) = serve_request(|path, body| {
        let request: Value = serde_json::from_slice(body).unwrap();
        match path {
            "/v1/remember" | "/v1/forget" => {
                let mut result = receipt(
                    "applied",
                    if path == "/v1/remember" {
                        request["scope"]["namespace"].as_str().unwrap()
                    } else {
                        "user"
                    },
                    path == "/v1/forget",
                );
                result["request_id"] = request["request_id"].clone();
                reply(200, result)
            }
            _ => reply(200, complete(vec![])),
        }
    })
    .await;
    let tools = tools(
        Arc::new(client(service(&url))),
        context(Origin::Model),
        8192,
    );
    let remember = tools
        .iter()
        .find(|tool| tool.definition().name == "remember")
        .unwrap();
    let forget = tools
        .iter()
        .find(|tool| tool.definition().name == "forget")
        .unwrap();
    let search = tools
        .iter()
        .find(|tool| tool.definition().name == "memory_search")
        .unwrap();
    let call = |value: Value| RawValue::from_string(value.to_string()).unwrap();
    for args in [
        json!({"text":"create","organization_id":"other"}),
        json!({"text":"org","scope":"organization"}),
        json!({"text":"update","target_id":"m-17","scope_namespace":"workspace","scope_id":"project-a","opaque_revision":"r4"}),
        json!({"text":"update","target_id":"m-17","organization_id":"acme","scope_namespace":"workspace","scope_id":"project-a"}),
        json!({"text":"update","target_id":"m-17","organization_id":"other","scope_namespace":"workspace","scope_id":"project-a","opaque_revision":"r4"}),
        json!({"text":"create","base_revision":1}),
    ] {
        assert!(
            remember
                .execute(&call(args), &CancellationToken::new())
                .await
                .is_error
        );
    }
    assert!(forget.execute(&call(json!({"id":"m-17","organization_id":"acme","scope_namespace":"organization","scope_id":"acme","opaque_revision":"r4"})),&CancellationToken::new()).await.is_error);
    assert!(search.execute(&call(json!({"id":"m-17","organization_id":"other","scope_namespace":"user","scope_id":"alice","opaque_revision":"r4"})),&CancellationToken::new()).await.is_error);
    assert!(seen.lock().unwrap().is_empty());
    let result = remember
        .execute(
            &call(json!({
                "text": "new value",
                "key": "editor.default",
                "target_id": "m-17",
                "organization_id": "acme",
                "scope_namespace": "workspace",
                "scope_id": "project-a",
                "opaque_revision": "r4",
            })),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert_eq!(sent["key"], "editor.default");
    assert_eq!(sent["target"], json!({"id":"m-17","revision":"r4"}));
    assert_eq!(sent["context"]["organization_id"], "acme");
    assert!(sent["metadata"].get("key").is_none());
    let result = remember
        .execute(
            &call(json!({"text":"unkeyed","key":null})),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[1].2).unwrap();
    assert!(sent["key"].is_null());
    let result = remember
        .execute(
            &call(json!({"text":"new preference","base_revision":0})),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[2].2).unwrap();
    assert_eq!(sent["text"], "new preference");
    assert!(sent["target"].is_null());
    let result = remember
        .execute(
            &call(json!({
                "base_revision": 0,
                "confidence": 1,
                "key": "verify-uncertainty-with-web-search",
                "kind": "preference",
                "opaque_revision": "",
                "organization_id": "",
                "scope": "user",
                "scope_namespace": "user",
                "text": "Search the web when uncertain"
            })),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[3].2).unwrap();
    assert_eq!(sent["scope"]["namespace"], "user");
    assert!(sent["target"].is_null());
}
