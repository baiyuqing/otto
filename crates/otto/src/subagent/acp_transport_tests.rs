use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

#[tokio::test]
async fn forwards_updates_and_denies_permission_requests() {
    let (client, agent) = duplex(4096);
    let (client_read, client_write) = tokio::io::split(client);
    let (agent_read, mut agent_write) = tokio::io::split(agent);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(agent_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["method"],
            "session/prompt"
        );
        agent_write.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"text\":\"hello\"}}\n").await.unwrap();
        agent_write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"session/request_permission\",\"params\":{}}\n").await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["result"]["outcome"]["outcome"],
            "cancelled"
        );
        agent_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"stopReason\":\"end_turn\"}}\n")
            .await
            .unwrap();
    });
    let mut connection = Connection::new(BufReader::new(client_read), client_write);
    let mut updates = Vec::new();
    let result = connection
        .request("session/prompt", json!({}), |value| updates.push(value))
        .await
        .unwrap();
    assert_eq!(updates[0]["text"], "hello");
    assert_eq!(result["stopReason"], "end_turn");
    server.await.unwrap();
}

#[tokio::test]
async fn reports_agent_disconnect_without_result() {
    let (client, agent) = duplex(4096);
    let (client_read, client_write) = tokio::io::split(client);
    let server = tokio::spawn(async move {
        let (agent_read, _) = tokio::io::split(agent);
        let mut reader = BufReader::new(agent_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
    });
    let mut connection = Connection::new(BufReader::new(client_read), client_write);
    let error = connection
        .request("initialize", json!({}), |_| {})
        .await
        .unwrap_err();
    assert!(error.contains("closed the connection"));
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_sends_session_cancel_and_waits_for_response() {
    let (client, agent) = duplex(4096);
    let (client_read, client_write) = tokio::io::split(client);
    let (agent_read, mut agent_write) = tokio::io::split(agent);
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(agent_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        server_cancel.cancel();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let message: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(message["method"], "session/cancel");
        assert_eq!(message["params"]["sessionId"], "session-a");
        agent_write
            .write_all(b"{\"id\":1,\"result\":{\"stopReason\":\"cancelled\"}}\n")
            .await
            .unwrap();
    });
    let mut connection = Connection::new(BufReader::new(client_read), client_write);
    let response = connection
        .request_with_cancel(
            "session/prompt",
            json!({}),
            Some((&cancel, "session-a")),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(response["stopReason"], "cancelled");
    server.await.unwrap();
}

#[tokio::test]
async fn permission_requests_are_bound_to_the_active_session() {
    struct Allow;
    #[async_trait::async_trait]
    impl PermissionHandler for Allow {
        async fn decide(&self, _: Value) -> Value {
            json!({"outcome":{"outcome":"selected","optionId":"yes"}})
        }
    }
    let (client, agent) = duplex(4096);
    let (client_read, client_write) = tokio::io::split(client);
    let (agent_read, mut agent_write) = tokio::io::split(agent);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(agent_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        for (id, session, expected) in [(50, "other", "cancelled"), (51, "right", "selected")] {
            let request = json!({"id":id,"method":"session/request_permission","params":{"sessionId":session}});
            agent_write
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["result"]["outcome"]["outcome"],
                expected
            );
        }
        agent_write
            .write_all(b"{\"id\":1,\"result\":{}}\n")
            .await
            .unwrap();
    });
    let mut connection = Connection::new(BufReader::new(client_read), client_write);
    connection.set_permission_handler("right".into(), Arc::new(Allow));
    connection
        .request("session/prompt", json!({}), |_| {})
        .await
        .unwrap();
    server.await.unwrap();
}
