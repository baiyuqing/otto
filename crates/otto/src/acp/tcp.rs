//! Authenticated loopback ACP transport owned by `otto serve`.
//!
//! The first line is `Authorization: Bearer <token>\n`; subsequent lines are
//! unchanged ACP v1 JSON-RPC frames. Authentication is bounded to 4096 bytes
//! and five seconds. Disconnect cancels this connection's turns, not sessions
//! or other clients. All operations use serve's existing router in process.
use std::io::{BufReader, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, header};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::server::{Server, auth, listen};

pub fn bind(address: &str, token: &str) -> Result<TcpListener, String> {
    if token.is_empty() || token.len() > 4000 || token.bytes().any(|b| !(33..=126).contains(&b)) {
        return Err(
            "serve: ACP requires a non-empty printable OTTO_ACP_TOKEN (maximum 4000 bytes)".into(),
        );
    }
    match listen::listen_tcp(address)? {
        listen::Listener::Tcp(listener) => Ok(listener),
        listen::Listener::Unix(..) => unreachable!(),
    }
}

async fn authenticate(stream: &mut TcpStream, token: &str) -> std::io::Result<bool> {
    let mut line = Vec::new();
    // Read only the preface; do not consume a pipelined initialize frame.
    for _ in 0..4096 {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            let Some(value) = line.strip_prefix(b"Authorization: ") else {
                return Ok(false);
            };
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_bytes(value) {
                headers.insert(header::AUTHORIZATION, value);
                return Ok(auth::authorized(token, &headers));
            }
            return Ok(false);
        }
        line.push(byte);
    }
    Ok(false)
}

pub async fn serve(
    listener: TcpListener,
    token: String,
    server: Arc<Server>,
    workspace: PathBuf,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let stop = cancel.child_token();
    let mut connections = JoinSet::new();
    let mut failure = None;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (mut stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => { failure = Some(format!("ACP accept: {error}")); break; }
                };
                // Bound readers and blocking protocol workers per listener.
                if connections.len() >= 64 { continue; }
                let token = token.clone();
                let server = Arc::clone(&server);
                let workspace = workspace.clone();
                let stop = stop.clone();
                connections.spawn(async move {
                    let authenticated = tokio::select! {
                        () = stop.cancelled() => false,
                        result = tokio::time::timeout(Duration::from_secs(5), authenticate(&mut stream, &token)) => {
                            matches!(result, Ok(Ok(true)))
                        }
                    };
                    if !authenticated { return; }
                    let Ok(mut stream) = stream.into_std() else { return; };
                    if stream.set_nonblocking(false).is_err() { return; }
                    let Ok(input) = stream.try_clone() else { return; };
                    let Ok(shutdown) = stream.try_clone() else { return; };
                    let runtime = tokio::runtime::Handle::current();
                    let connection_stop = stop.child_token();
                    let worker_stop = connection_stop.clone();
                    let mut worker = tokio::task::spawn_blocking(move || {
                        runtime.block_on(super::attach::serve_in_process(
                            server, workspace, Box::new(BufReader::new(input)), &mut stream, &worker_stop,
                        ));
                        let _ = stream.flush();
                        let _ = stream.shutdown(Shutdown::Both);
                    });
                    tokio::select! {
                        () = stop.cancelled() => {
                            connection_stop.cancel();
                            let _ = shutdown.shutdown(Shutdown::Both);
                            // The listener awaits protocol workers before closing serve.
                            let _ = worker.await;
                        }
                        _ = &mut worker => {}
                    }
                    // Also wake the reader if the protocol worker panicked.
                    let _ = shutdown.shutdown(Shutdown::Both);
                });
            }
        }
    }
    stop.cancel();
    while connections.join_next().await.is_some() {}
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn authentication_rejects_bad_tokens_and_preserves_first_frame() {
        let listener = bind("127.0.0.1:0", "test-token").unwrap();
        for (preface, expected) in [
            ("Authorization: Bearer wrong\n", false),
            ("Authorization: Bearer test-token\n{}\n", true),
        ] {
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            client.write_all(preface.as_bytes()).await.unwrap();
            assert_eq!(
                authenticate(&mut server, "test-token").await.unwrap(),
                expected
            );
            if expected {
                assert_eq!(server.read_u8().await.unwrap(), b'{');
            }
        }
        assert!(bind("0.0.0.0:0", "test-token").is_err());
        assert!(bind("127.0.0.1:0", "").is_err());
        assert!(bind("127.0.0.1:0", "bad\ntoken").is_err());
    }
}
