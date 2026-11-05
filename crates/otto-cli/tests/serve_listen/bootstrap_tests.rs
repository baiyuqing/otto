//! Offline first-run configuration bootstrap over the normal listener.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_home_serves_config_and_requires_setup_before_sessions() {
    let mut fixture = fixture("http://127.0.0.1:1", "");
    std::fs::remove_file(&fixture.config).expect("remove initial config");
    fixture.config.clear(); // Normal first-run discovery, not an explicit missing --config.
    fixture
        .environment
        .retain(|entry| !entry.starts_with(b"TEST_KEY="));
    let (base, token, handle) = spawn_bootstrap(&fixture).await;
    let client = otto::tls_provider::http_client();
    let unauth = client
        .get(format!("{base}/v1/config/providers"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
    let authorized = |method, path: &str| {
        client
            .request(method, format!("{base}{path}"))
            .bearer_auth(&token)
    };
    let providers = authorized(reqwest::Method::GET, "/v1/config/providers")
        .send()
        .await
        .unwrap();
    assert_eq!(providers.status().as_u16(), 200);
    let providers: serde_json::Value = response_json(providers).await;
    assert_eq!(providers["default_model"], "");
    assert!(providers["providers"].as_array().unwrap().len() >= 3);
    let catalog = authorized(reqwest::Method::GET, "/v1/config/models?provider=chatgpt")
        .send()
        .await
        .unwrap();
    assert_eq!(catalog.status().as_u16(), 501);
    assert_eq!(
        response_json(catalog).await["error"]["code"],
        "models_unsupported"
    );
    for body in ["{}", r#"{"request_id":"bootstrap-new"}"#] {
        let reply = authorized(reqwest::Method::POST, "/v1/sessions")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status().as_u16(), 503);
        let reply: serde_json::Value = response_json(reply).await;
        assert_eq!(reply["error"]["code"], "configuration_required");
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("restart")
        );
    }
    for body in [
        r#"{"kind":"set_provider","provider":"local","type":"openai-compatible","base_url":"http://127.0.0.1:1","api_key_env":"BOOTSTRAP_KEY"}"#,
        r#"{"kind":"set_model","model":"local/test-model"}"#,
        r#"{"kind":"set_default_model","model":"local/test-model"}"#,
    ] {
        let preview = authorized(reqwest::Method::POST, "/v1/config/changes")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(preview.status().as_u16(), 200);
        let preview: serde_json::Value = response_json(preview).await;
        let id = preview["id"].as_str().unwrap();
        let confirm = authorized(reqwest::Method::POST, &format!("/v1/config/changes/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(confirm.status().as_u16(), 200);
        assert_eq!(
            response_json(confirm).await["status"],
            "saved_restart_required"
        );
    }
    // Saving config deliberately does not replace the running workspace builder.
    let session = authorized(reqwest::Method::POST, "/v1/sessions")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(session.status().as_u16(), 503);
    stop_bootstrap(handle).await;

    // Restart with the configured selector and an offline dummy key; no turn/provider request.
    fixture
        .environment
        .push(b"BOOTSTRAP_KEY=offline-not-a-real-key".to_vec());
    let (base, token, handle) = spawn_bootstrap(&fixture).await;
    let session = client
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        session.status().as_u16(),
        201,
        "{}",
        session.text().await.unwrap()
    );
    stop_bootstrap(handle).await;
}

async fn response_json(response: reqwest::Response) -> serde_json::Value {
    serde_json::from_str(&response.text().await.unwrap()).unwrap()
}

async fn spawn_bootstrap(fixture: &Fixture) -> (String, String, tokio::process::Child) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .args(arguments(fixture, &["--listen", "127.0.0.1:0"]))
        .env_clear();
    for entry in &fixture.environment {
        let entry = std::str::from_utf8(entry).unwrap();
        let (name, value) = entry.split_once('=').unwrap();
        command.env(name, value);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("startup line");
    let url = line.strip_prefix("otto serve: ").expect("serve URL");
    let (base, token) = url.split_once("/?token=").expect("token URL");
    (base.into(), token.into(), child)
}

async fn stop_bootstrap(mut child: tokio::process::Child) {
    let status = std::process::Command::new("/bin/kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
