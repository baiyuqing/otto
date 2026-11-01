//! Session interchange and composition parity between an embedded `Otto`
//! and `otto acp` run in-process through the CLI entry point.

use otto::fixtures::embed_harness::{
    Capture, Fixture, Installed, builder, environment, fixture, reply, say, walk,
};
use otto::session::workspace_key;
use tokio_util::sync::CancellationToken;

/// `build()` is exclusive per process, so every test that builds an `Otto`
/// holds this for its whole body.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One `NAME=value` environment entry as the CLI entry point takes it.
fn entry(name: &str, value: &str) -> Vec<u8> {
    format!("{name}={value}").into_bytes()
}

/// Blocking stdin for `otto acp` that the test feeds one line at a time,
/// because a request needs the session id the previous reply returned.
struct Lines {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    position: usize,
}

impl std::io::Read for Lines {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let available = std::io::BufRead::fill_buf(self)?;
        let count = available.len().min(out.len());
        out[..count].copy_from_slice(&available[..count]);
        std::io::BufRead::consume(self, count);
        Ok(count)
    }
}

impl std::io::BufRead for Lines {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.position >= self.buffer.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.buffer = next;
                    self.position = 0;
                }
                Err(_) => return Ok(&[]),
            }
        }
        Ok(&self.buffer[self.position..])
    }
    fn consume(&mut self, count: usize) {
        self.position += count;
    }
}

/// `otto acp --cwd <workspace>` run in-process through the CLI entry
/// point, with the fixture's home as the whole environment.
struct Acp {
    input: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    stdout: Capture,
    stderr: Capture,
    task: tokio::task::JoinHandle<i32>,
    next: u64,
}

impl Acp {
    fn start(fixture: &Fixture) -> Self {
        let (input, rx) = std::sync::mpsc::channel();
        let (stdout, stderr) = (Capture::default(), Capture::default());
        let (mut out, mut err) = (stdout.clone(), stderr.clone());
        let args = vec![
            "acp".to_string(),
            "--cwd".to_string(),
            fixture.workspace.to_string_lossy().into_owned(),
        ];
        let entries: Vec<Vec<u8>> = environment(fixture)
            .iter()
            .map(|(name, value)| entry(name, value))
            .collect();
        let task = tokio::spawn(async move {
            otto_cli::run::run(
                &args,
                Box::new(Lines {
                    rx,
                    buffer: Vec::new(),
                    position: 0,
                }),
                &mut out,
                &mut err,
                entries,
                false,
                &CancellationToken::new(),
                &otto_cli::terminate::Terminate::new(),
            )
            .await
        });
        Self {
            input: Some(input),
            stdout,
            stderr,
            task,
            next: 0,
        }
    }

    /// Sends one request and returns the `result` of its reply, or panics
    /// with the error the reply carries.
    async fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next += 1;
        let id = self.next;
        let frame =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let line = format!("{frame}\n");
        self.input
            .as_ref()
            .unwrap()
            .send(line.into_bytes())
            .unwrap();
        for _ in 0..600 {
            for line in self.stdout.text().lines() {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if value["id"] == id && value.get("method").is_none() {
                    assert!(value.get("error").is_none(), "{method}: {value}");
                    return value["result"].clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("no reply to {method}; stdout: {}", self.stdout.text());
    }

    async fn session(&mut self, fixture: &Fixture) -> String {
        self.call("initialize", serde_json::json!({"protocolVersion": 1}))
            .await;
        let reply = self
            .call(
                "session/new",
                serde_json::json!({"cwd": fixture.workspace, "mcpServers": []}),
            )
            .await;
        reply["sessionId"].as_str().expect("session id").to_string()
    }

    async fn prompt(&mut self, session: &str, text: &str) {
        self.call(
            "session/prompt",
            serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
        .await;
    }

    /// Ends stdin and returns the exit code and stderr.
    async fn finish(mut self) -> (i32, String) {
        self.input = None;
        let code = self.task.await.expect("acp task");
        (code, self.stderr.text())
    }
}

#[tokio::test]
async fn sessions_made_by_otto_and_by_the_cli_open_in_each_other() {
    let _serial = LOCK.lock().await;
    let fixture = fixture("");

    let (provider, _script) = Installed::new(Vec::new());
    provider.serve(&fixture);
    let otto = builder(&fixture).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    say(&session, "hello from otto").await;
    let from_otto = session.id().to_string();
    drop(session);
    otto.close().await.unwrap();

    let mut acp = Acp::start(&fixture);
    let from_cli = acp.session(&fixture).await;
    acp.prompt(&from_cli, "hello from the cli").await;
    let listed = acp
        .call(
            "session/list",
            serde_json::json!({"cwd": fixture.workspace}),
        )
        .await
        .to_string();
    assert!(
        listed.contains(&from_otto),
        "the CLI lists the Otto session: {listed}"
    );
    acp.call(
        "session/load",
        serde_json::json!({"sessionId": from_otto, "cwd": fixture.workspace, "mcpServers": []}),
    )
    .await;
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");

    let otto = builder(&fixture).build().await.unwrap();
    let summaries = otto.list_sessions(None, 10).await.unwrap();
    for id in [&from_otto, &from_cli] {
        assert!(summaries.iter().any(|s| &s.id == id), "Otto lists {id}");
    }
    let resumed = otto.resume_session(&from_cli, None).await.unwrap();
    assert!(
        format!("{:?}", resumed.history()).contains("hello from the cli"),
        "Otto resumes the CLI session with its messages"
    );
    drop(resumed);
    otto.close().await.unwrap();
}

/// The files under `~/.otto` and the session root of a fixture, as
/// relative paths with session, workspace and diagnostic runtime ids replaced.
fn tree(fixture: &Fixture, session: &str) -> Vec<String> {
    // Session directories and workflow locks are named by this key.
    let workspace = workspace_key(&fixture.workspace).unwrap();
    let mut names: Vec<String> = walk(&fixture.home)
        .iter()
        .filter(|path| {
            let name = path.to_string_lossy();
            !name.ends_with("-wal") && !name.ends_with("-shm") && !name.contains("/memory/")
        })
        .map(|path| {
            let relative = path.strip_prefix(&fixture.home).unwrap();
            let mut parts: Vec<String> = relative
                .components()
                .map(|part| {
                    part.as_os_str()
                        .to_string_lossy()
                        .replace(session, "<session>")
                        .replace(&workspace, "<workspace>")
                })
                .collect();
            // Each runtime owns a random diagnostic family. Keep suffixes
            // and duplicate entries so missing or extra files still differ.
            if parts.len() == 3
                && parts[0] == ".otto"
                && parts[1] == "logs"
                && let Some((family, suffix)) = parts[2].split_once('.')
                && family.starts_with("diagnostic-")
            {
                parts[2] = format!("diagnostic-<runtime>.{suffix}");
            }
            parts.join("/")
        })
        .collect();
    names.sort();
    names
}

#[test]
fn tree_normalization_preserves_file_kinds_and_counts() {
    let fixture = fixture("");
    let logs = fixture.home.join(".otto/logs");
    std::fs::create_dir_all(&logs).unwrap();
    for family in [
        "diagnostic-123-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "diagnostic-456-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    ] {
        for suffix in ["jsonl", "jsonl.1", "lock"] {
            std::fs::write(logs.join(format!("{family}.{suffix}")), "").unwrap();
        }
    }
    std::fs::write(logs.join("other.jsonl"), "").unwrap();
    let locks = fixture.home.join(".otto/workflow-locks");
    std::fs::create_dir_all(&locks).unwrap();
    let key = workspace_key(&fixture.workspace).unwrap();
    for name in [key.as_str(), "0000000000000000"] {
        std::fs::write(locks.join(format!("{name}.lock")), "").unwrap();
    }
    assert_eq!(
        tree(&fixture, "unused-session"),
        vec![
            ".config/otto/config.toml",
            ".otto/logs/diagnostic-<runtime>.jsonl",
            ".otto/logs/diagnostic-<runtime>.jsonl",
            ".otto/logs/diagnostic-<runtime>.jsonl.1",
            ".otto/logs/diagnostic-<runtime>.jsonl.1",
            ".otto/logs/diagnostic-<runtime>.lock",
            ".otto/logs/diagnostic-<runtime>.lock",
            ".otto/logs/other.jsonl",
            ".otto/workflow-locks/0000000000000000.lock",
            ".otto/workflow-locks/<workspace>.lock",
        ]
    );
}

fn normalize(text: &str, fixture: &Fixture, session: &str) -> String {
    text.replace(&*fixture.root.to_string_lossy(), "<root>")
        .replace(session, "<session>")
}

/// For one config, environment and prompt, a default `Otto` and
/// `otto acp` send the provider the same request (model, system prompt
/// and tool definitions), write the same diagnostics, and leave the same
/// files under the home. A behavior of one composition that the provider
/// request, the diagnostics and the file tree do not reflect is not
/// compared.
#[tokio::test]
async fn a_default_otto_matches_otto_acp() {
    let _serial = LOCK.lock().await;
    let (provider, script) = Installed::new(vec![reply("one"), reply("two")]);

    let cli = fixture("");
    provider.serve(&cli);
    let mut acp = Acp::start(&cli);
    let cli_session = acp.session(&cli).await;
    acp.prompt(&cli_session, "parity").await;
    let (code, cli_stderr) = acp.finish().await;
    assert_eq!(code, 0, "{cli_stderr}");

    let embedded = fixture("");
    provider.serve(&embedded);
    let capture = Capture::default();
    let otto = builder(&embedded)
        .diagnostics(Box::new(capture.clone()))
        .build()
        .await
        .unwrap();
    let session = otto.new_session(None).await.unwrap();
    say(&session, "parity").await;
    let otto_session = session.id().to_string();
    drop(session);
    otto.close().await.unwrap();
    drop(provider);

    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let (from_cli, from_otto) = (&requests[0], &requests[1]);
    assert!(
        from_cli.tools.len() > 20,
        "the request carries the tool set"
    );
    assert_eq!(from_cli.tools, from_otto.tools, "tool definitions");
    assert_eq!(from_cli.model, from_otto.model);
    assert_eq!(from_cli.thinking, from_otto.thinking);
    assert_eq!(
        normalize(&from_cli.system_prompt, &cli, &cli_session),
        normalize(&from_otto.system_prompt, &embedded, &otto_session),
        "system prompt"
    );
    // The in-process `otto acp` writes its process log through
    // `Logger::stderr()` to the real stderr, not to `cli_stderr`, so only
    // the embedded capture holds `time=` log lines.
    let diagnostics = capture.text();
    assert!(
        diagnostics.contains("msg=provider_operation_finished"),
        "{diagnostics}"
    );
    let warnings: String = diagnostics
        .lines()
        .filter(|line| !line.starts_with("time="))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(
        normalize(&cli_stderr, &cli, &cli_session),
        normalize(&warnings, &embedded, &otto_session),
        "diagnostics"
    );
    assert_eq!(
        tree(&cli, &cli_session),
        tree(&embedded, &otto_session),
        "files under the home"
    );
}
