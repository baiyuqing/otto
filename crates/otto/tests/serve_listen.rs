//! `otto serve` over a real loopback listener.
//!
//! The whole startup path runs in-process, so the listener, the token gate,
//! the session factory and the embedded UI are all exercised against a bound
//! socket rather than a `oneshot` service.

use std::io::Write;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

mod common;
use common::{Script, serve, text_reply};

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
            "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\n\
             base_url = \"{base_url}\"\nmodel = \"gpt-test\"\napi_key_env = \"TEST_KEY\"\n{extra_config}"
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
        otto::cli::run::run(
            &args,
            Box::new(std::io::Cursor::new(Vec::new())),
            &mut out,
            &mut err,
            environment,
            false,
            &cancel,
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
        if let Some(line) = text.strip_prefix("otto serve: ")
            && line.ends_with('\n')
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
    let base_url = serve(Script {
        replies: vec![text_reply("served")],
        served: Arc::new(AtomicUsize::new(0)),
    });
    let fixture = fixture(&base_url, "");
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) =
        run_serve(&fixture, &["--listen", "127.0.0.1:0"], cancel.clone()).await;
    let (base, token) = await_startup(&stdout, &stderr).await;
    let client = reqwest::Client::new();

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
    let fixture = fixture(
        "http://127.0.0.1:1",
        "\n[server]\nlisten = \"localhost:0\"\n",
    );
    let cancel = CancellationToken::new();
    let (stdout, stderr, handle) = run_serve(&fixture, &[], cancel.clone()).await;
    let (base, _token) = await_startup(&stdout, &stderr).await;

    let page = reqwest::get(format!("{base}/")).await.expect("root page");
    assert_eq!(page.status().as_u16(), 200);

    cancel.cancel();
    let code = tokio::time::timeout(Duration::from_secs(7), handle)
        .await
        .expect("serve stops")
        .expect("serve task");
    assert_eq!(code, 0, "stderr {:?}", stderr.text());
}
