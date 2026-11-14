//! `otto serve` over a real loopback listener.
//!
//! The whole startup path runs in-process, so the listener, the token gate,
//! the session factory and the API are all exercised against a bound
//! socket rather than a `oneshot` service.

use std::io::Write;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

const MAX_CONCURRENT_SERVE_FIXTURES: usize = 4;
static SERVE_FIXTURE_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_SERVE_FIXTURES);

async fn serve_fixture_permit() -> tokio::sync::SemaphorePermit<'static> {
    SERVE_FIXTURE_PERMITS
        .acquire()
        .await
        .expect("serve fixture semaphore remains open")
}

mod binary_harness;
#[path = "serve_listen/bootstrap_tests.rs"]
mod bootstrap_tests;
use otto::fixtures::scripted_provider::{Script, serve, text_reply, tool_call_reply};

/// Lets a test read serve's stdout while the server task is still writing.
#[derive(Clone, Default)]
struct LockedBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LockedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LockedBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|poison| poison.into_inner()))
            .into_owned()
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    config: String,
    workspace: String,
    environment: Vec<Vec<u8>>,
}

/// A HOME, a workspace and a config naming `base_url` as the only profile.
fn fixture(base_url: &str, extra_config: &str) -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let config_dir = home.path().join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let config = config_dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "{}{extra_config}",
            binary_harness::model_config(base_url, "TEST_KEY")
        ),
    )
    .expect("write config");
    let environment = [
        format!("HOME={}", home.path().display()),
        "SHELL=/bin/sh".to_string(),
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        "TEST_KEY=sk-serve-not-a-real-key".to_string(),
    ]
    .iter()
    .map(|entry| entry.as_bytes().to_vec())
    .collect();
    Fixture {
        config: config.to_string_lossy().into_owned(),
        workspace: workspace.path().to_string_lossy().into_owned(),
        environment,
        _home: home,
        _workspace: workspace,
    }
}

fn arguments(fixture: &Fixture, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "serve".to_string(),
        "--config".to_string(),
        fixture.config.clone(),
        "--cwd".to_string(),
        fixture.workspace.clone(),
    ];
    if fixture.config.is_empty() {
        args.drain(1..3);
    }
    args.extend(extra.iter().map(|value| value.to_string()));
    args
}

async fn run_serve(
    fixture: &Fixture,
    extra: &[&str],
    cancel: CancellationToken,
) -> (LockedBuffer, LockedBuffer, tokio::task::JoinHandle<i32>) {
    let stdout = LockedBuffer::default();
    let stderr = LockedBuffer::default();
    let args = arguments(fixture, extra);
    let environment = fixture.environment.clone();
    let (mut out, mut err) = (stdout.clone(), stderr.clone());
    let handle = tokio::spawn(async move {
        otto_cli::run::run(
            &args,
            Box::new(std::io::Cursor::new(Vec::new())),
            &mut out,
            &mut err,
            environment,
            false,
            &cancel,
            &otto_cli::terminate::Terminate::new(),
        )
        .await
    });
    (stdout, stderr, handle)
}

/// Waits for the `otto serve:` startup line and returns the base URL and the
/// token parsed from it.
async fn await_startup(stdout: &LockedBuffer, stderr: &LockedBuffer) -> (String, String) {
    for _ in 0..400 {
        let text = stdout.text();
        if text.ends_with('\n')
            && let Some(line) = text.lines().find_map(|line| {
                line.strip_prefix("otto serve: http://")
                    .map(|rest| format!("http://{rest}"))
            })
        {
            let line = line.trim();
            let rest = line
                .strip_prefix("http://")
                .unwrap_or_else(|| panic!("startup URL {line:?}: want an http:// origin"));
            let (host, query) = rest
                .split_once("/?token=")
                .unwrap_or_else(|| panic!("startup URL {line:?}: want /?token=<32 hex>"));
            assert!(host.starts_with("127.0.0.1:"), "startup URL {line:?}");
            assert_eq!(query.len(), 32, "startup URL {line:?}");
            assert!(query.chars().all(|c| c.is_ascii_hexdigit()), "{line:?}");
            return (format!("http://{host}"), query.to_string());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "no startup line on stdout; stdout {:?} stderr {:?}",
        stdout.text(),
        stderr.text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_refuses_a_non_loopback_listen_address() {
    let _serve_fixture_permit = serve_fixture_permit().await;
    let fixture = fixture("http://127.0.0.1:1", "");
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) = run_serve(&fixture, &["--listen", "0.0.0.0:0"], cancel).await;
    let code = handle.await.expect("serve task");
    assert_eq!(code, 1, "stderr {:?}", stderr.text());
    assert!(stderr.text().contains("loopback"), "{:?}", stderr.text());
    assert_eq!(stdout.text(), "", "nothing is printed before a bind fails");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_listens_on_loopback_tcp_behind_the_token() {
    let _serve_fixture_permit = serve_fixture_permit().await;
    let (base_url, _requests) = serve(Script {
        replies: vec![text_reply("served")],
        served: Arc::new(AtomicUsize::new(0)),
    });
    let fixture = fixture(&base_url, "");
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) =
        run_serve(&fixture, &["--listen", "127.0.0.1:0"], cancel.clone()).await;
    let (base, token) = await_startup(&stdout, &stderr).await;
    let client = otto::tls_provider::http_client();

    let health = client
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("healthz");
    assert_eq!(health.status().as_u16(), 200);

    let refused = client
        .post(format!("{base}/v1/sessions"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("unauthenticated create");
    assert_eq!(refused.status().as_u16(), 401);

    let authed = |method: reqwest::Method, path: String, body: &'static str| {
        client
            .request(method, format!("{base}{path}"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body)
            .send()
    };

    let created = authed(reqwest::Method::POST, "/v1/sessions".to_string(), "{}")
        .await
        .expect("create");
    assert_eq!(created.status().as_u16(), 201);
    let created: serde_json::Value =
        serde_json::from_str(&created.text().await.expect("create body")).expect("create json");
    let id = created["id"].as_str().expect("an id").to_string();

    let turn = authed(
        reqwest::Method::POST,
        format!("/v1/sessions/{id}/turns"),
        r#"{"text":"hello","stream":false}"#,
    )
    .await
    .expect("turn");
    let turn: serde_json::Value =
        serde_json::from_str(&turn.text().await.expect("turn body")).expect("turn json");
    assert_eq!(turn["text"], "served", "stderr {:?}", stderr.text());

    let resumed = client
        .post(format!("{base}/v1/sessions"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(format!("{{\"resume\":\"{id}\"}}"))
        .send()
        .await
        .expect("resume");
    assert_eq!(resumed.status().as_u16(), 200);

    cancel.cancel();
    let code = tokio::time::timeout(Duration::from_secs(7), handle)
        .await
        .expect("serve stops")
        .expect("serve task");
    assert_eq!(code, 0, "stderr {:?}", stderr.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_listen_address_can_come_from_the_config_file() {
    let _serve_fixture_permit = serve_fixture_permit().await;
    let fixture = fixture(
        "http://127.0.0.1:1",
        "\n[server]\nlisten = \"localhost:0\"\n",
    );
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) = run_serve(&fixture, &[], cancel.clone()).await;
    let (base, _token) = await_startup(&stdout, &stderr).await;

    let page = otto::tls_provider::http_client()
        .get(format!("{base}/"))
        .send()
        .await
        .expect("root request");
    assert_eq!(page.status().as_u16(), 404);

    cancel.cancel();
    let code = tokio::time::timeout(Duration::from_secs(7), handle)
        .await
        .expect("serve stops")
        .expect("serve task");
    assert_eq!(code, 0, "stderr {:?}", stderr.text());
}

/// One HTTP/1.1 request over the unix socket; returns the status code.
async fn socket_status(socket: &std::path::Path, path: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .expect("connect to the socket");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: otto\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("write");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.expect("read");
    reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("status line in {reply:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socket_and_tcp_listeners_serve_one_server_with_the_token_on_tcp_only() {
    let _serve_fixture_permit = serve_fixture_permit().await;
    let fixture = fixture("http://127.0.0.1:1", "");
    let sockets = tempfile::tempdir().expect("socket dir");
    let socket = sockets.path().join("run").join("otto.sock");
    let socket_arg = socket.to_string_lossy().into_owned();
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) = run_serve(
        &fixture,
        &["--socket", &socket_arg, "--listen", "127.0.0.1:0"],
        cancel.clone(),
    )
    .await;
    let (base, token) = await_startup(&stdout, &stderr).await;
    let client = otto::tls_provider::http_client();

    assert_eq!(socket_status(&socket, "/v1/sessions").await, 200);

    let refused = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("tcp without token");
    assert_eq!(refused.status().as_u16(), 401);
    let allowed = client
        .get(format!("{base}/v1/sessions"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("tcp with token");
    assert_eq!(allowed.status().as_u16(), 200);

    cancel.cancel();
    let code = tokio::time::timeout(Duration::from_secs(7), handle)
        .await
        .expect("serve stops")
        .expect("serve task");
    assert_eq!(code, 0, "stderr {:?}", stderr.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_queued_followup_runs_after_the_task_with_agent_provenance() {
    let _serve_fixture_permit = serve_fixture_permit().await;
    use serde_json::Value;

    let (provider, requests) = serve(Script {
        replies: vec![
            tool_call_reply(
                "queue-one",
                "task_queue",
                r#"{"prompt":"write the final report"}"#,
            ),
            text_reply("Research completed."),
            text_reply("Final report completed."),
        ],
        served: Arc::new(AtomicUsize::new(0)),
    });
    let fixture = fixture(&provider, "");
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) = run_serve(
        &fixture,
        &["--sandbox", "off", "--listen", "127.0.0.1:0"],
        cancel.clone(),
    )
    .await;
    let (base, token) = await_startup(&stdout, &stderr).await;
    let client = otto::tls_provider::http_client();
    let created = client
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let created: Value = serde_json::from_str(&created).unwrap();
    let id = created["id"].as_str().unwrap();
    let result = client
        .post(format!("{base}/v1/sessions/{id}/turns"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(r#"{"text":"research this topic","stream":false}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    let text = result["text"].as_str().unwrap_or_default();
    assert_eq!(
        text, "Research completed.\n\nFinal report completed.",
        "{result}"
    );
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 3);
        let followup: Value = serde_json::from_str(&captured[2]).unwrap();
        let content = followup["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(
            content.ends_with("[Agent follow-up]\nwrite the final report"),
            "{content}"
        );
    }

    let history = client
        .get(format!("{base}/v1/sessions/{id}/history"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let history: Value = serde_json::from_str(&history).unwrap();
    let history_text = history.to_string();
    assert!(history_text.contains("agent_followup"), "{history}");
    cancel.cancel();
    assert_eq!(handle.await.unwrap(), 0, "{}", stderr.text());
}
