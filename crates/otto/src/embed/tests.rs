use super::*;
use crate::runtime::builder::TEST_PROVIDERS;
use otto_core::{
    agent::Event,
    model::{BlockType, FinishReason, Role},
    operation::OperationControl,
    provider::{Provider, ProviderSettlement, Request, Response, StreamSink},
};
use serde_json::value::RawValue;
use std::collections::VecDeque;

/// `build()` is exclusive per process, so every test that builds an
/// `Otto` holds this for its whole body.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
}

const PROFILE: &str = r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"
"#;

/// A temp home with a user config of `PROFILE`, `extra` and
/// `[sandbox] driver = "off"`, and an empty workspace.
fn fixture(extra: &str) -> Fixture {
    let directory = tempfile::tempdir().expect("tempdir");
    let root = directory.path().canonicalize().expect("canonical root");
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let extra = extra.replace("{ROOT}", &root.to_string_lossy());
    std::fs::write(
        home.join(".config/otto/config.toml"),
        format!("{PROFILE}\n{extra}\n[sandbox]\ndriver = \"off\"\n"),
    )
    .expect("write config");
    Fixture {
        _directory: directory,
        root,
        home,
        workspace,
    }
}

fn environment(fixture: &Fixture) -> Vec<(String, String)> {
    vec![
        (
            "HOME".to_string(),
            fixture.home.to_string_lossy().into_owned(),
        ),
        ("ALPHA_KEY".to_string(), "sk-alpha".to_string()),
    ]
}

fn builder(fixture: &Fixture) -> OttoBuilder {
    Otto::builder()
        .workspace(&fixture.workspace)
        .environment(environment(fixture))
}

/// A diagnostics writer the test can read back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Replays `replies` in order and records every request.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Message>>,
    requests: Mutex<Vec<Request>>,
}

#[async_trait::async_trait]
impl Provider for Script {
    async fn complete(
        &self,
        request: &Request,
        _: StreamSink<'_>,
        _: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.requests.lock().unwrap().push(request.clone());
        let message = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| reply("done"));
        ProviderSettlement::succeeded(
            Response {
                message,
                ..Response::default()
            },
            1,
        )
    }
}

fn reply(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        finish_reason: Some(FinishReason::Stop),
        ..Message::default()
    }
}

fn tool_call(name: &str, arguments: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "call-1".into(),
            tool_name: name.into(),
            arguments: Some(RawValue::from_string(arguments.into()).unwrap()),
            ..Block::default()
        }],
        finish_reason: Some(FinishReason::ToolCalls),
        ..Message::default()
    }
}

/// A scripted provider that runners built under a served fixture's home
/// use until dropped.
struct Installed(Arc<Script>);

impl Installed {
    fn new(replies: Vec<Message>) -> (Self, Arc<Script>) {
        let script = Arc::new(Script {
            replies: Mutex::new(replies.into()),
            ..Script::default()
        });
        (Self(script.clone()), script)
    }

    /// Makes runners built under `fixture`'s home use this script.
    fn serve(&self, fixture: &Fixture) {
        TEST_PROVIDERS
            .lock()
            .unwrap()
            .push((fixture.home.to_string_lossy().into_owned(), self.0.clone()));
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        TEST_PROVIDERS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(_, provider)| {
                !std::ptr::addr_eq(Arc::as_ptr(provider), Arc::as_ptr(&self.0))
            });
    }
}

async fn deny(_: ApprovalRequest) -> ApprovalDecision {
    ApprovalDecision::Deny
}

async fn say(session: &Session, text: &str) -> Stop {
    session
        .prompt(
            Input::Text(text),
            &mut |_| {},
            &CancellationToken::new(),
            deny,
        )
        .await
        .expect("prompt")
}

/// The tool names of a new session, with `disabled` switched off, and the
/// entry names under the test's `~/.otto` after the runtime closed.
async fn observe(fixture: &Fixture, disabled: &[Component]) -> (Vec<String>, Vec<String>) {
    let mut options = builder(fixture);
    for component in disabled {
        options = options.disable(*component);
    }
    let otto = options.build().await.expect("build");
    let session = otto.new_session(None).await.expect("session");
    let tools = session.controller.tool_names();
    drop(session);
    otto.close().await.expect("close");
    let mut entries: Vec<String> = std::fs::read_dir(fixture.home.join(".otto"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    (tools, entries)
}

fn has(names: &[String], wanted: &[&str]) -> bool {
    wanted.iter().all(|name| names.iter().any(|n| n == name))
}

fn lacks(names: &[String], unwanted: &[&str]) -> bool {
    unwanted.iter().all(|name| !names.iter().any(|n| n == name))
}

/// Runs the switch for `component` against a control without it:
/// `tools` appear in the control and not with the switch; `files` are
/// created by the control and not with the switch.
async fn switch(component: Component, extra: &str, tools: &[&str], files: &[&str]) {
    let _serial = LOCK.lock().await;
    let (control_tools, control_files) = observe(&fixture(extra), &[]).await;
    assert!(
        has(&control_tools, tools),
        "control tools {control_tools:?}"
    );
    for file in files {
        assert!(
            control_files.iter().any(|f| f == file),
            "control files {control_files:?} lack {file}"
        );
    }
    let (tools_off, files_off) = observe(&fixture(extra), &[component]).await;
    assert!(
        lacks(&tools_off, tools),
        "{component:?} tools {tools_off:?}"
    );
    for file in files {
        assert!(
            !files_off.iter().any(|f| f == file),
            "{component:?} files {files_off:?} hold {file}"
        );
    }
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

mod disabling;
mod lifecycle;
mod parity;
