use super::*;

#[tokio::test]
async fn session_info_is_strict_read_only_and_returns_only_identity() {
    let tool = SessionInfo {
        session_id: "runtime-only".into(),
    };
    let definition = tool.definition();
    let schema: serde_json::Value =
        serde_json::from_str(definition.parameters.as_ref().unwrap().get()).unwrap();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"], json!({}));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(tool.retry_safety(), otto_core::model::RetrySafety::ReadOnly);
    let cancel = CancellationToken::new();
    let result = tool
        .execute(&RawValue::from_string("{}".into()).unwrap(), &cancel)
        .await;
    assert!(!result.is_error);
    assert_eq!(result.content, r#"{"session_id":"runtime-only"}"#);
    for arguments in [
        "null",
        "[]",
        "1",
        r#"{"session_id":"spoof"}"#,
        r#"{"path":"secret"}"#,
    ] {
        let result = tool
            .execute(&RawValue::from_string(arguments.into()).unwrap(), &cancel)
            .await;
        assert!(result.is_error, "accepted {arguments}");
    }
    cancel.cancel();
    assert!(
        tool.execute(&RawValue::from_string("{}".into()).unwrap(), &cancel)
            .await
            .is_error
    );
}
