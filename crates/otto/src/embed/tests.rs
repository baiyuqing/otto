
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
        ("OTTO_MEM_TOKEN".to_string(), "mem-test-token".to_string()),
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

#[tokio::test]
async fn bash_off_removes_the_tool_and_reports_bash_unavailable() {
    switch(Component::Bash, "", &["bash"], &[]).await;
    let _serial = LOCK.lock().await;
    let fixture = fixture("");
    let otto = builder(&fixture)
        .disable(Component::Bash)
        .build()
        .await
        .unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert!(!session.info().sandbox.bash_available);
    otto.close().await.unwrap();
    let control = builder(&fixture).build().await;
    assert!(control.is_ok(), "bash stays available without the switch");
    let control = control.unwrap();
    assert!(
        control
            .new_session(None)
            .await
            .unwrap()
            .info()
            .sandbox
            .bash_available
    );
    control.close().await.unwrap();
}

#[tokio::test]
async fn memory_off_removes_the_memory_tools() {
    switch(
        Component::Memory,
        "",
        &["memory_search", "remember", "forget"],
        &[],
    )
    .await;
}

#[tokio::test]
async fn subagents_off_removes_the_subagent_tools() {
    switch(
        Component::Subagents,
        "",
        &[
            "agent",
            "agent_status",
            "agent_yield",
            "agent_send",
            "agent_cancel",
        ],
        &[],
    )
    .await;
}

#[tokio::test]
async fn reflection_off_removes_the_tool_and_the_store() {
    switch(Component::Reflection, "", &["reflect"], &["reflection.db"]).await;
}

#[tokio::test]
async fn schedule_off_does_not_create_the_store_and_keeps_the_tools() {
    switch(Component::Schedule, "", &[], &["schedule.db"]).await;
    let _serial = LOCK.lock().await;
    let (tools, _) = observe(&fixture(""), &[Component::Schedule]).await;
    assert!(has(
        &tools,
        &["schedule", "schedule_list", "schedule_cancel"]
    ));
}

#[tokio::test]
async fn usage_off_does_not_create_the_store() {
    switch(Component::Usage, "", &[], &["usage.db"]).await;
}

#[tokio::test]
async fn task_history_off_does_not_create_the_store() {
    switch(Component::TaskHistory, "", &[], &["tasks.db"]).await;
}

/// An MCP server that records that it was spawned, then exits.
const MARKER_SERVER: &str = r#"
[mcp]
enabled = true

[mcp.servers.marker]
transport = "stdio"
command = "/bin/sh"
args = ["-c", "/usr/bin/touch '{ROOT}/spawned'"]
"#;

async fn spawned(fixture: &Fixture) -> bool {
    let marker = fixture.root.join("spawned");
    for _ in 0..40 {
        if marker.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn mcp_off_does_not_spawn_a_configured_server() {
    let _serial = LOCK.lock().await;
    let control = fixture(MARKER_SERVER);
    let otto = builder(&control).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert!(spawned(&control).await, "the control spawns its server");
    drop(session);
    otto.close().await.unwrap();

    let off = fixture(MARKER_SERVER);
    let otto = builder(&off).disable(Component::Mcp).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert!(
        !spawned(&off).await,
        "the switch leaves the server unspawned"
    );
    drop(session);
    otto.close().await.unwrap();
}

/// `[mcp].enabled = true` in config does not beat the switch, in the
/// startup workspace or in one opened later.
#[tokio::test]
async fn disable_wins_over_config_in_every_workspace() {
    let _serial = LOCK.lock().await;
    let extra = format!("{MARKER_SERVER}\n[server]\nworkspace_roots = [\"{{ROOT}}/projects\"]\n");
    let second = |fixture: &Fixture| {
        let path = fixture.root.join("projects/two");
        std::fs::create_dir_all(&path).unwrap();
        path
    };

    let control = fixture(&extra);
    let two = second(&control);
    let otto = builder(&control).build().await.unwrap();
    let session = otto.new_session(Some(&two)).await.unwrap();
    assert!(
        spawned(&control).await,
        "the second workspace spawns without the switch"
    );
    drop(session);
    otto.close().await.unwrap();

    let off = fixture(&extra);
    let two = second(&off);
    let otto = builder(&off).disable(Component::Mcp).build().await.unwrap();
    otto.new_session(None).await.unwrap();
    let session = otto.new_session(Some(&two)).await.unwrap();
    assert!(!spawned(&off).await);
    let canonical = two.canonicalize().unwrap();
    let loaded = otto.inner.loaded.lock().await;
    let (builder, _) = loaded.get(&canonical).expect("workspace was loaded");
    assert!(
        !builder.mcp.enabled,
        "the second workspace config has MCP off"
    );
    drop(loaded);
    drop(session);
    otto.close().await.unwrap();
}

#[tokio::test]
async fn skills_off_removes_the_skill_catalog() {
    let _serial = LOCK.lock().await;
    let skill = |fixture: &Fixture| {
        let directory = fixture.home.join(".otto/skills/demo");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\nbody\n",
        )
        .unwrap();
    };
    let (provider, script) = Installed::new(vec![reply("one"), reply("two")]);
    for (off, expected) in [(false, true), (true, false)] {
        let fixture = fixture("");
        provider.serve(&fixture);
        skill(&fixture);
        let mut options = builder(&fixture);
        if off {
            options = options.disable(Component::Skills);
        }
        let otto = options.build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        say(&session, "hello").await;
        let request = script.requests.lock().unwrap().pop().unwrap();
        let listed = request.system_prompt.contains("demo skill")
            || request.tools.iter().any(|tool| tool.name == "skill");
        assert_eq!(listed, expected, "skills off = {off}");
        drop(session);
        otto.close().await.unwrap();
    }
}

/// The plugin catalog needs an installed package, which no offline
/// fixture creates; this pins that the switch reaches the resolved
/// config, which is where the plugin wiring reads it.
#[tokio::test]
async fn plugins_off_sets_the_resolved_config_key() {
    let _serial = LOCK.lock().await;
    for (off, expected) in [(false, true), (true, false)] {
        let fixture = fixture("");
        let mut options = builder(&fixture);
        if off {
            options = options.disable(Component::Plugins);
        }
        let otto = options.build().await.unwrap();
        assert_eq!(
            otto.inner.builder.config.plugins.enabled.unwrap_or(true),
            expected
        );
        otto.close().await.unwrap();
    }
}

#[tokio::test]
async fn session_persistence_off_writes_no_session_file_and_refuses_resume_and_list() {
    let _serial = LOCK.lock().await;
    let (provider, _script) = Installed::new(Vec::new());
    let count = |otto: &Otto| -> usize {
        let root = &otto.inner.builder.session_root;
        walk(root)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .count()
    };
    let fixture_on = fixture("");
    provider.serve(&fixture_on);
    let otto = builder(&fixture_on).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    say(&session, "hello").await;
    assert_eq!(count(&otto), 1, "the control writes one session file");
    drop(session);
    otto.close().await.unwrap();

    let fixture_off = fixture("");
    provider.serve(&fixture_off);
    let otto = builder(&fixture_off)
        .disable(Component::SessionPersistence)
        .build()
        .await
        .unwrap();
    let session = otto.new_session(None).await.unwrap();
    say(&session, "hello").await;
    assert_eq!(
        session.history().len(),
        2,
        "the conversation is kept in memory"
    );
    assert_eq!(count(&otto), 0);
    assert!(matches!(
        otto.list_sessions(None, 10).await,
        Err(Error::PersistenceDisabled)
    ));
    assert!(matches!(
        otto.resume_session(&"0".repeat(32), None).await,
        Err(Error::PersistenceDisabled)
    ));
    drop(session);
    otto.close().await.unwrap();
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

#[tokio::test]
async fn only_one_otto_runs_per_process() {
    let _serial = LOCK.lock().await;
    let fixture = fixture("");
    let first = builder(&fixture).build().await.unwrap();
    assert!(matches!(
        builder(&fixture).build().await,
        Err(Error::AlreadyRunning)
    ));
    let clone = first.clone();
    first.close().await.unwrap();
    assert!(
        matches!(first.new_session(None).await, Err(Error::Closed)),
        "a closed runtime refuses new sessions"
    );
    first.close().await.expect("a second close is a no-op");
    drop(first);
    drop(clone);
    builder(&fixture)
        .build()
        .await
        .expect("a build after close succeeds")
        .close()
        .await
        .unwrap();

    let dropped = builder(&fixture).build().await.unwrap();
    drop(dropped);
    builder(&fixture)
        .build()
        .await
        .expect("a build after the last clone dropped succeeds")
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn a_prompt_that_needs_approval_calls_decide_and_a_denial_ends_the_turn() {
    let _serial = LOCK.lock().await;
    let (provider, script) = Installed::new(vec![
        tool_call(
            "request_approval",
            r#"{"title":"Publish","reason":"push branch","proposal":"git push"}"#,
        ),
        reply("understood"),
    ]);
    let fixture = fixture("");
    provider.serve(&fixture);
    let otto = builder(&fixture).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Auto);
    session.set_auto_mode(false).unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Manual);
    // The embedding API selects every mode. Assert it, then put the
    // session back in MANUAL: this round trip is about `decide` receiving
    // the request, and YOLO would approve it without calling `decide`.
    session.set_approval_mode(ApprovalMode::Yolo).unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Yolo);
    session.set_approval_mode(ApprovalMode::Manual).unwrap();
    assert_eq!(session.info().approval_mode, ApprovalMode::Manual);
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let asked = Arc::clone(&seen);
    let stop = session
        .prompt(
            Input::Text("push it"),
            &mut move |event| sink.lock().unwrap().push(event),
            &CancellationToken::new(),
            move |request| {
                asked.lock().unwrap().push(request);
                async { ApprovalDecision::Deny }
            },
        )
        .await
        .expect("prompt");
    assert!(matches!(stop, Stop::EndTurn));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "decide runs once");
        assert!(
            seen[0].command.contains("git push"),
            "{:?}",
            seen[0].command
        );

        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "the denial result reaches the model");
        let result = requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .find(|block| block.block_type == BlockType::ToolResult)
            .expect("a tool result block");
        assert!(
            result.text.contains("\"approved\":false")
                || result.text.contains("\"approved\": false"),
            "{}",
            result.text
        );

        let events = events.lock().unwrap();
        let position = |wanted: fn(&Event) -> bool| events.iter().position(wanted);
        let started = position(|e| matches!(e, Event::AgentStarted)).expect("AgentStarted");
        let call = position(|e| matches!(e, Event::ToolCallStarted { .. })).expect("call started");
        let finished =
            position(|e| matches!(e, Event::ToolCallFinished { .. })).expect("call finished");
        let done = position(|e| matches!(e, Event::AgentFinished)).expect("AgentFinished");
        assert!(started < call && call < finished && finished < done);
    }
    drop(session);
    otto.close().await.unwrap();
}

/// The startup warnings the CLI test pins in
/// `startup_warnings_keep_their_order_and_text`, written to the
/// diagnostics writer.
#[tokio::test]
async fn diagnostics_receive_the_startup_warnings_the_cli_writes_to_stderr() {
    let _serial = LOCK.lock().await;
    let fixture = fixture("[memory]\nenabled = false\n[mcp]\nenabled = false\n");
    for store in ["usage.db", "tasks.db", "schedule.db"] {
        std::fs::create_dir_all(fixture.home.join(".otto").join(store)).unwrap();
    }
    std::fs::create_dir_all(fixture.workspace.join(".otto")).unwrap();
    std::fs::write(
        fixture.workspace.join(".otto/config.toml"),
        "[sandbox]\ndriver = \"seatbelt\"\n",
    )
    .unwrap();
    let capture = Capture::default();
    let otto = builder(&fixture)
        .diagnostics(Box::new(capture.clone()))
        .build()
        .await
        .unwrap();
    otto.close().await.unwrap();
    let text = capture
        .text()
        .replace(&*fixture.workspace.to_string_lossy(), "<workspace>");
    assert_eq!(
        text,
        "warning: sandbox in <workspace>/.otto/config.toml: field is restricted to user or managed configuration\n\
             warning: usage store unavailable, continuing without usage history\n\
             warning: task history store unavailable, continuing without task recording\n\
             warning: schedule store unavailable, continuing without scheduled deliveries\n\
             warning: sandbox is off; bash runs unsandboxed as your user\n"
    );
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
            crate::cli::run::run(
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
                &crate::cli::terminate::Terminate::new(),
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
                })
                .collect();
            // `.otto/sessions/<hash of the workspace path>/<id>.jsonl`
            if parts.len() == 4 && parts[1] == "sessions" {
                parts[2] = "<workspace>".to_string();
            }
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
fn diagnostic_tree_normalization_preserves_file_kinds_and_counts() {
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

/// A remote memory service stub. `/v1/turns` waits 200 ms, records the
/// body and answers `202`; every other endpoint answers `503`.
async fn memory_service() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let turns = Arc::new(Mutex::new(Vec::new()));
    let seen = turns.clone();
    let app = axum::Router::new()
        .route(
            "/v1/turns",
            axum::routing::post(move |body: axum::body::Bytes| {
                let seen = seen.clone();
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    seen.lock()
                        .unwrap()
                        .push(serde_json::from_slice(&body).unwrap());
                    axum::response::Response::builder()
                        .status(202)
                        .header("Otto-Memory-Protocol", "1.0")
                        .body(axum::body::Body::from("{}"))
                        .unwrap()
                }
            }),
        )
        .fallback(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, turns)
}

/// Checks the one uploaded turn of a prompt `asked` answered by `answer`.
fn assert_uploaded(turns: &[serde_json::Value], session: &str, asked: &str, answer: &str) {
    assert_eq!(turns.len(), 1, "{turns:?}");
    let turn = &turns[0];
    assert_eq!(turn["status"], "success");
    assert_eq!(turn["context"]["session_id"], session);
    assert_eq!(turn["context"]["organization_id"], "acme");
    assert_eq!(turn["context"]["user_id"], "u1");
    assert!(turn["context"].get("scopes").is_none());
    let messages = turn["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert!(
        messages[0]["blocks"][0]["text"]
            .as_str()
            .unwrap()
            .contains(asked),
        "{turn}"
    );
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["blocks"][0]["text"], answer);
}

/// A finished prompt reaches the remote memory service as one `/v1/turns`
/// request, and shutdown returns only after that request was answered:
/// through `Otto::close` and through `otto acp` reaching end of input.
#[tokio::test]
async fn finished_prompts_are_uploaded_to_the_remote_memory_service_before_shutdown() {
    let _serial = LOCK.lock().await;
    let (url, turns) = memory_service().await;
    let config = format!(
        "[memory]\nenabled = true\nbackend = \"remote\"\n\n[memory.remote]\norganization = \"acme\"\nurl = \"{url}\"\nuser = \"u1\"\n"
    );
    let (provider, _) = Installed::new(vec![reply("embedded answer"), reply("acp answer")]);

    let embedded = fixture(&config);
    provider.serve(&embedded);
    let otto = builder(&embedded).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    say(&session, "upload this embedded turn").await;
    let session_id = session.id().to_string();
    drop(session);
    otto.close().await.unwrap();
    assert_uploaded(
        &std::mem::take(&mut *turns.lock().unwrap()),
        &session_id,
        "upload this embedded turn",
        "embedded answer",
    );

    let cli = fixture(&config);
    provider.serve(&cli);
    let mut acp = Acp::start(&cli);
    let session_id = acp.session(&cli).await;
    acp.prompt(&session_id, "upload this acp turn").await;
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
    assert_uploaded(
        &turns.lock().unwrap(),
        &session_id,
        "upload this acp turn",
        "acp answer",
    );
}
