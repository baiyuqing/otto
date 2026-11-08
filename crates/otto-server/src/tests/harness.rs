//! Test harness: server construction, request helpers and the SSE reader.

use super::*;

// ---- the harness ----

/// A `Vec<u8>` the test reads back after the logger wrote to it.
pub(super) struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct HarnessOptions {
    pub(super) script: Script,
    pub(super) fixed_id: Option<String>,
    pub(super) create_error: Option<String>,
    pub(super) open_error: Option<String>,
    pub(super) open_gate: Option<CancellationToken>,
    pub(super) list: Option<ListResult>,
    pub(super) list_by_workspace: HashMap<String, ListResult>,
    pub(super) tasks: Option<Arc<otto::subagent::tasks::Tasks>>,
    pub(super) task_recorder: Option<Arc<otto::subagent::record::Store>>,
    pub(super) workflows: Option<Arc<otto::workflow::Controller>>,
    /// Extra workflow controllers keyed by workspace path, for a second
    /// (non-startup) workspace's workflow routes.
    pub(super) workflow_workspaces: HashMap<String, Arc<otto::workflow::Controller>>,
    /// `startup` is always overwritten with the harness's own workspace;
    /// set `admitted`/`error`/`gate` for `/v1/workspaces` tests.
    pub(super) workspaces: FakeWorkspaces,
    pub(super) sandbox_reload: Option<Result<SandboxInfo, String>>,
    pub(super) sandbox_reload_others: Vec<(String, Result<SandboxInfo, String>)>,
    /// The process sandbox every controller is wired to, which
    /// `amend_sandbox` reloads.
    pub(super) sandbox_control: Option<Arc<dyn otto::app::SandboxControl>>,
    /// Canned `Factory::diff_runner` answer for the startup workspace.
    pub(super) diff_runner: Option<(Arc<dyn otto::sandbox::CommandExecutor>, Vec<String>)>,
    /// The startup workspace directory. Defaults to a fresh, empty temp
    /// directory; a test that runs real git commands (through a real
    /// `CommandExecutor`) sets this to the directory it initialized a
    /// repository in.
    pub(super) workspace: Option<TempDir>,
    pub(super) token: String,
    pub(super) remote_listener: bool,
    pub(super) info: Info,
    /// Initial config.toml bytes for config-management route tests.
    pub(super) config_text: Option<String>,
    pub(super) disabled_skills: Vec<String>,
    pub(super) skills_enabled: Option<bool>,
    pub(super) skill_path: Option<std::path::PathBuf>,
    /// Registers the approval store and the `bash` double that
    /// [`Script::tool_command`] calls.
    pub(super) elevate: bool,
    /// The scheduled-delivery store behind `/v1/schedule/*`.
    pub(super) schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>,
    /// Backs every session with a real session file instead of memory.
    pub(super) persistent: bool,
    /// `[server].session_idle_timeout` for the harness's server, as the
    /// resolved runtime holds it. `None` (the default) never closes a
    /// session for being idle.
    pub(super) session_idle_timeout: Option<Duration>,
    /// `[server].max_open_sessions` for the harness's server. `None` (the
    /// default) keeps every open session.
    pub(super) max_open_sessions: Option<usize>,
    /// Wires a file memory store in this directory into every session.
    pub(super) memory_store: Option<std::path::PathBuf>,
}

pub(super) struct Harness {
    pub(super) _workspace: TempDir,
    pub(super) _sessions: TempDir,
    pub(super) server: Arc<Server>,
    pub(super) router: Router,
    pub(super) factory: Arc<TestFactory>,
    pub(super) provider: Arc<ScriptedProvider>,
    pub(super) log: Arc<Mutex<Vec<u8>>>,
}

impl Harness {
    pub(super) fn new() -> Self {
        Self::with(HarnessOptions::default())
    }

    pub(super) async fn restart(&mut self) {
        self.server.close().await.expect("close server");
        let server = Server::new(Options {
            factory: Arc::clone(&self.factory) as Arc<dyn Factory>,
            info: Info {
                workspace: self.factory.builder.workspace_path.clone(),
                ..Info::default()
            },
            config_path: self.factory.builder.shared.config_path.clone(),
            token: String::new(),
            remote_listener: false,
            logger: Some(Arc::new(Logger::discard())),
            workflows: None,
            schedule: None,
            session_idle_timeout: None,
            max_open_sessions: None,
        });
        self.router = server.router();
        self.server = server;
    }

    pub(super) fn with(options: HarnessOptions) -> Self {
        let workspace = options
            .workspace
            .unwrap_or_else(|| tempfile::tempdir().expect("workspace"));
        let sessions = tempfile::tempdir().expect("sessions");
        // Canonicalized so a real `sandbox::Executor` bound to the same
        // directory (a test's `diff_runner` against a real git
        // repository) accepts it: `Executor::valid_directory` requires
        // an already-canonical path, and macOS's `$TMPDIR` is a symlink.
        let canonical_workspace = std::fs::canonicalize(workspace.path())
            .unwrap_or_else(|_| workspace.path().to_path_buf());
        let mut builder = fixtures::builder(&canonical_workspace, sessions.path());
        if let Some(skill_path) = options.skill_path {
            builder.shared_mut().config.skills.paths =
                Some(vec![skill_path.to_string_lossy().into_owned()]);
        }
        builder
            .shared_mut()
            .config
            .skills
            .disabled
            .extend(options.disabled_skills);
        if options.elevate {
            let executor = otto::sandbox::Executor::new(
                Arc::new(otto::sandbox::direct::DirectDriver::new()),
                otto::sandbox::Policy {
                    filesystem: otto::sandbox::FilesystemMode::Unconfined,
                    network: otto::sandbox::NetworkMode::Allow,
                },
                &canonical_workspace,
            )
            .expect("executor");
            builder.bash_approvals = Some(Arc::new(otto::tool::bash::BashApprovals::new(
                Arc::new(executor),
                Vec::new(),
            )));
        }
        builder.shared_mut().config.skills.enabled = options.skills_enabled;
        if let Some(config_text) = &options.config_text {
            std::fs::write(&builder.shared.config_path, config_text).expect("write config fixture");
        }
        builder.shared_mut().schedule = options.schedule.clone();
        if let Some(store) = &options.memory_store {
            fixtures::wire_file_memory(&mut builder, &canonical_workspace, store);
        }
        let builder = Arc::new(builder);
        let provider = ScriptedProvider::new(options.script);
        // Production always sets `Options.info.workspace` from the same
        // path as the startup workspace (`cli/serve.rs`); match that here
        // unless a test explicitly overrides `info` to check the field
        // itself (e.g. `the_info_endpoint_echoes_the_configured_info`).
        let mut info = options.info;
        if info.workspace.is_empty() {
            info.workspace = builder.workspace_path.clone();
        }
        let mut workspaces = options.workspaces;
        workspaces.startup = builder.workspace_path.clone();
        let mut workflows = options.workflow_workspaces;
        if let Some(controller) = &options.workflows {
            workflows.insert(builder.workspace_path.clone(), Arc::clone(controller));
        }
        let factory = Arc::new(TestFactory {
            builder,
            provider: Arc::clone(&provider),
            fixed_id: options.fixed_id,
            create_error: options.create_error,
            open_error: options.open_error,
            open_gate: options.open_gate,
            list: options.list,
            list_by_workspace: options.list_by_workspace,
            tasks: options.tasks,
            task_recorder: options.task_recorder,
            workspaces,
            workflows,
            sandbox_reload: options.sandbox_reload,
            sandbox_reload_others: options.sandbox_reload_others,
            sandbox_control: options.sandbox_control,
            diff_runner: options.diff_runner,
            persistent: options.persistent,
            create_calls: AtomicUsize::new(0),
            open_calls: AtomicUsize::new(0),
        });
        let log = Arc::new(Mutex::new(Vec::new()));
        let server = Server::new(Options {
            factory: Arc::clone(&factory) as Arc<dyn Factory>,
            info,
            config_path: factory.builder.shared.config_path.clone(),
            token: options.token,
            remote_listener: options.remote_listener,
            logger: Some(Arc::new(Logger::new(Box::new(SharedSink(Arc::clone(
                &log,
            )))))),
            workflows: options.workflows,
            schedule: options.schedule,
            session_idle_timeout: options.session_idle_timeout,
            max_open_sessions: options.max_open_sessions,
        });
        let router = server.router();
        Self {
            _workspace: workspace,
            _sessions: sessions,
            server,
            router,
            factory,
            provider,
            log,
        }
    }

    /// One request through the real router. `tower`'s `oneshot` needs no
    /// socket, so these tests run with no network access at all.
    pub(super) async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        headers: &[(&str, &[u8])],
    ) -> Response {
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request
            .body(body.map_or_else(Body::empty, |text| Body::from(text.to_string())))
            .expect("request");
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("infallible")
    }

    /// One `GET` through the router that serves the Unix socket.
    pub(super) async fn send_on_socket(&self, path: &str) -> Reply {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("request");
        let response = self
            .server
            .socket_router()
            .oneshot(request)
            .await
            .expect("infallible");
        let status = response.status();
        let head = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        Reply {
            status,
            headers: head,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    pub(super) async fn send(&self, method: &str, path: &str, body: Option<&str>) -> Reply {
        self.send_with(method, path, body, &[]).await
    }

    pub(super) async fn send_with(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        headers: &[(&str, &[u8])],
    ) -> Reply {
        let response = self.raw(method, path, body, headers).await;
        let status = response.status();
        let head = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        Reply {
            status,
            headers: head,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    pub(super) async fn create(&self) -> String {
        let reply = self.send("POST", "/v1/sessions", None).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.json()["id"].as_str().expect("id").to_string()
    }

    pub(super) fn logged(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap_or_else(|poison| poison.into_inner()))
            .into_owned()
    }

    pub(super) async fn metrics_body(&self) -> String {
        self.send("GET", "/metrics", None).await.body
    }

    /// Polls the turn until it leaves `running`.
    pub(super) async fn wait_turn_done(&self, session: &str, turn_id: &str) -> Value {
        for _ in 0..600 {
            let body = self
                .send(
                    "GET",
                    &format!("/v1/sessions/{session}/turns/{turn_id}"),
                    None,
                )
                .await
                .json();
            if body["status"] != turn::TURN_RUNNING && body["status"] != turn::TURN_QUEUED {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("turn {turn_id} did not finish in time");
    }

    /// Waits for the turn object itself, for the paths that have no HTTP
    /// route left (a deleted session, a closed server).
    pub(super) async fn wait_done(turn: &Arc<Turn>) {
        for _ in 0..600 {
            if turn.is_done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("turn did not finish in time");
    }

    pub(super) fn active_turn(&self, id: &str) -> Arc<Turn> {
        self.server
            .lookup(id)
            .and_then(|session| session.current_turn())
            .expect("an active turn")
    }
}

pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: String,
}

impl Reply {
    pub(super) fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("decode {:?}: {error}", self.body))
    }

    pub(super) fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }
}

// ---- the SSE reader ----

/// Reads frames off a streaming body with the same splitter the browser
/// client uses ([`parse_frames`]), so a framing change fails here too.
pub(super) struct SseReader {
    pub(super) stream: axum::body::BodyDataStream,
    pub(super) rest: String,
    pub(super) pending: std::collections::VecDeque<Frame>,
}

impl SseReader {
    pub(super) fn new(response: Response) -> Self {
        Self {
            stream: response.into_body().into_data_stream(),
            rest: String::new(),
            pending: std::collections::VecDeque::new(),
        }
    }

    pub(super) async fn next(&mut self) -> Option<Frame> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }
            let chunk = self.stream.next().await?.expect("chunk");
            self.rest.push_str(&String::from_utf8_lossy(&chunk));
            let parsed = parse_frames(&self.rest);
            self.rest = parsed.rest;
            self.pending.extend(parsed.frames);
        }
    }

    pub(super) async fn all(mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next().await {
            frames.push(frame);
        }
        frames
    }
}

/// Starts a streaming turn and returns its reader plus the turn id, read
/// out of the `agent_started` frame that follows the leading `user_message` frame.
pub(super) async fn start_stream(harness: &Harness, session: &str) -> (SseReader, String) {
    let response = harness
        .raw(
            "POST",
            &format!("/v1/sessions/{session}/turns"),
            Some(r#"{"text":"hi"}"#),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let header_id = response
        .headers()
        .get("otto-turn-id")
        .and_then(|value| value.to_str().ok())
        .expect("Otto-Turn-Id header")
        .to_string();
    let mut reader = SseReader::new(response);
    let user = reader.next().await.expect("user_message frame");
    assert_eq!(user.event, "user_message");
    assert_eq!(user.id, Some(0));
    let first = reader.next().await.expect("agent_started frame");
    assert_eq!(first.event, "agent_started");
    assert_eq!(first.id, Some(1));
    let data: Value = serde_json::from_str(&first.data).expect("frame data");
    let turn_id = data["turn_id"].as_str().expect("turn_id").to_string();
    assert_eq!(turn_id, header_id);
    (reader, turn_id)
}

pub(super) fn gated() -> (CancellationToken, HarnessOptions) {
    let gate = CancellationToken::new();
    (
        gate.clone(),
        HarnessOptions {
            script: Script {
                // One delta so the buffer holds a sequence 1 while the
                // gate is shut; several tests resume from it.
                deltas: vec!["...".to_string()],
                gate: Some(gate),
                ..Script::default()
            },
            ..HarnessOptions::default()
        },
    )
}
