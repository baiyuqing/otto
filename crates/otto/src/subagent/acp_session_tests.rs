use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

#[tokio::test]
async fn creates_session_and_delivers_only_its_updates() {
    let (client, agent) = duplex(8192);
    let (client_read, client_write) = tokio::io::split(client);
    let (agent_read, mut agent_write) = tokio::io::split(agent);
    let server =
        tokio::spawn(async move {
            let mut reader = BufReader::new(agent_read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["method"],
                "initialize"
            );
            agent_write
                .write_all(b"{\"id\":1,\"result\":{\"protocolVersion\":1}}\n")
                .await
                .unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["params"]["cwd"],
                "/work"
            );
            agent_write
                .write_all(b"{\"id\":2,\"result\":{\"sessionId\":\"session-a\"}}\n")
                .await
                .unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["params"]["prompt"][0]["text"],
                "hello"
            );
            agent_write.write_all(concat!(
            "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"other\"}}\n",
            "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"session-a\"}}\n",
            "{\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}\n"
        ).as_bytes()).await.unwrap();
        });
    let mut session = Session::open(BufReader::new(client_read), client_write, "/work")
        .await
        .unwrap();
    let mut updates = Vec::new();
    let result = session
        .prompt("hello", |update| updates.push(update))
        .await
        .unwrap();
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["sessionId"], "session-a");
    server.await.unwrap();
}
