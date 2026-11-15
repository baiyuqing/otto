use super::*;
use crate::fixtures::plugin_package::Fixture;
use otto_core::config::File;
use std::fs;
fn package(fixture: &Fixture, source: &str) -> PathBuf {
    let path = fixture.package_with_extension(
        "lua-demo",
        json!({
            "version":1,
            "hooks":{"before_prompt":[{"lua":"handler.lua"}]},
            "tools":[{
                "name":"calculate",
                "description":"Double a number; returns the calculated number as text.",
                "input_schema":{
                    "type":"object", "properties":{"number":{"type":"integer"}},
                    "required":["number"], "additionalProperties":false
                },
                "lua":"handler.lua"
            }]
        }),
    );
    fs::write(path.join("handler.lua"), source).unwrap();
    path
}
fn runtime(fixture: &Fixture) -> Arc<Runtime> {
    Runtime::new(
        &crate::plugin::Snapshot::load(fixture.home.path(), &File::default(), &fixture.env()),
        fixture.home.path(),
        &Default::default(),
        Execution {
            executor: None,
            environment: vec!["SENSITIVE=never-exposed".into()],
            package_environment: [(
                "lua-demo".into(),
                vec![("BOUND".into(), "never-exposed".into())],
            )]
            .into(),
            workspace: fixture.home.path().to_owned(),
            guard: None,
            secrets: vec!["never-exposed".into()],
            logger: Arc::new(otto_platform::log::Logger::discard()),
        },
    )
    .unwrap()
}
#[tokio::test]
async fn trusted_package_hook_tool_schema_snapshot_and_revoke_without_executor() {
    let fixture = Fixture::new();
    let path = package(
        &fixture,
        "local input=...; if input.event=='tool' then return {content=tostring(input.arguments.number*2)} end; return {context='Lua hook context'}",
    );
    let installed = fixture.install(&path);
    assert!(!installed.trusted);
    assert!(runtime(&fixture).tools().is_empty());
    fixture.service().change("lua-demo", "trust").unwrap();
    let runtime = runtime(&fixture);
    let tools = runtime.tools();
    assert_eq!(tools[0].definition().name, "plugin__lua-demo__calculate");
    let args = serde_json::value::RawValue::from_string(r#"{"number":21}"#.into()).unwrap();
    let approval = tools[0].approval_spec(&args).unwrap().unwrap();
    assert!(approval.details.contains("handler.lua"));
    fs::write(
        path.join("handler.lua"),
        "error('changed source must not run')",
    )
    .unwrap();
    let result = tools[0].execute(&args, &CancellationToken::new()).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "42");
    let invalid = serde_json::value::RawValue::from_string(r#"{"number":"bad"}"#.into()).unwrap();
    assert!(
        tools[0]
            .execute(&invalid, &CancellationToken::new())
            .await
            .is_error
    );
    let hook = runtime
        .command(
            super::Handler {
                command: &[],
                lua: Some("handler.lua"),
                timeout: "",
            },
            runtime.packages.first(),
            json!({"event":"before_prompt"}),
            &CancellationToken::new(),
            None,
        )
        .await
        .map_err(|error| error.message)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&hook).unwrap()["context"],
        "Lua hook context"
    );
    fixture.service().change("lua-demo", "revoke").unwrap();
    assert!(
        tools[0]
            .execute(&args, &CancellationToken::new())
            .await
            .is_error
    );
    assert!(
        runtime
            .command(
                super::Handler {
                    command: &[],
                    lua: Some("handler.lua"),
                    timeout: "",
                },
                runtime.packages.first(),
                json!({}),
                &CancellationToken::new(),
                None
            )
            .await
            .is_err()
    );
}
#[test]
fn rejects_invalid_handlers_sources_and_unknown_manifest_fields() {
    for path in [
        "../escape.lua",
        "/absolute.lua",
        "./handler.lua",
        "a/../handler.lua",
        "",
        "handler.so",
    ] {
        assert!(
            lua_handler::validate(&[], Some(path), "").is_err(),
            "{path}"
        );
    }
    assert!(lua_handler::validate(&["sh".into()], Some("handler.lua"), "").is_err());
    assert!(lua_handler::validate(&[], None, "").is_err());
    let fixture = Fixture::new();
    let path = package(&fixture, "return {}");
    for source in [b"\x1bLua".as_slice(), &[255]] {
        fs::write(path.join("handler.lua"), source).unwrap();
        assert!(
            fixture
                .service()
                .install(path.to_str().unwrap(), None)
                .is_err()
        );
    }
    fs::write(
        path.join("handler.lua"),
        vec![b' '; lua_engine::MAX_SOURCE as usize + 1],
    )
    .unwrap();
    assert!(
        fixture
            .service()
            .install(path.to_str().unwrap(), None)
            .is_err()
    );
    fs::remove_file(path.join("handler.lua")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", path.join("handler.lua")).unwrap();
    assert!(
        fixture
            .service()
            .install(path.to_str().unwrap(), None)
            .is_err()
    );
    assert!(serde_json::from_value::<Hook>(json!({"lua":"handler.lua","unexpected":1})).is_err());
    let old: Hook = serde_json::from_value(json!({"command":["echo"]})).unwrap();
    assert!(old.lua.is_none());
    assert!(!serde_json::to_string(&old).unwrap().contains("lua"));
    let standalone = otto_core::config::plugins::Hooks {
        before_prompt: vec![Hook {
            lua: Some("handler.lua".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let empty = runtime(&fixture);
    assert!(
        Runtime::new(
            &Default::default(),
            fixture.home.path(),
            &standalone,
            empty.execution.clone()
        )
        .is_err()
    );
}
#[tokio::test]
async fn example_package_hook_denies_and_tool_runs_without_host_capabilities() {
    let fixture = Fixture::new();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/lua-demo");
    fixture.install(&example);
    fixture.service().change("lua-demo", "trust").unwrap();
    let runtime = runtime(&fixture);
    let hooks = runtime.bind(super::tests::binding(), None);
    let rejected = hooks
        .run(
            "before_tool",
            json!({"tool":"plugin__lua-demo__calculate", "arguments":{"number":-1}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.decision, Some(Decision::Deny));
    let accepted = hooks
        .run(
            "before_tool",
            json!({"tool":"plugin__lua-demo__calculate", "arguments":{"number":21}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(accepted.decision.is_none());
    let tools = runtime.tools();
    let arguments = serde_json::value::RawValue::from_string(r#"{"number":21}"#.into()).unwrap();
    assert_eq!(
        tools[0]
            .execute(&arguments, &CancellationToken::new())
            .await
            .content,
        "42"
    );
    let installed = fixture.service().inspect("lua-demo").unwrap();
    let snapshot = fixture.service().package_path(&installed.digest);
    crate::plugin::thaw(&snapshot).unwrap();
    fs::write(
        snapshot.join("calculate.lua"),
        "return {content='tampered'}",
    )
    .unwrap();
    assert!(
        tools[0]
            .execute(&arguments, &CancellationToken::new())
            .await
            .is_error
    );
    assert!(fixture.service().change("lua-demo", "trust").is_err());
}
#[tokio::test]
async fn revoke_during_an_invocation_never_delivers_a_result() {
    let fixture = Fixture::new();
    let path = package(&fixture, "while true do end");
    fixture.install(&path);
    fixture.service().change("lua-demo", "trust").unwrap();
    let runtime = runtime(&fixture);
    let token = CancellationToken::new();
    let future = runtime.command(
        super::Handler {
            command: &[],
            lua: Some("handler.lua"),
            timeout: "10s",
        },
        runtime.packages.first(),
        json!({}),
        &token,
        None,
    );
    let revoke = async {
        tokio::task::yield_now().await;
        fixture.service().change("lua-demo", "revoke").unwrap();
    };
    let (result, ()) = tokio::join!(future, revoke);
    assert!(result.is_err());
    assert!(!runtime.valid(runtime.packages.first()));
}
#[tokio::test]
async fn cancelled_and_tiny_deadline_handlers_terminate_and_return_errors() {
    let fixture = Fixture::new();
    let path = package(&fixture, "while true do end");
    fixture.install(&path);
    fixture.service().change("lua-demo", "trust").unwrap();
    let runtime = runtime(&fixture);
    let token = CancellationToken::new();
    let cancel = token.clone();
    let future = runtime.command(
        super::Handler {
            command: &[],
            lua: Some("handler.lua"),
            timeout: "10s",
        },
        runtime.packages.first(),
        json!({}),
        &token,
        None,
    );
    let task = async {
        tokio::task::yield_now().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(future, task);
    assert!(result.is_err());
    assert!(
        runtime
            .command(
                super::Handler {
                    command: &[],
                    lua: Some("handler.lua"),
                    timeout: "1ns",
                },
                runtime.packages.first(),
                json!({}),
                &CancellationToken::new(),
                None
            )
            .await
            .is_err()
    );
}

#[derive(Default)]
pub(super) struct Observer {
    pub running: tokio::sync::Notify,
    pub exited: tokio::sync::Notify,
    pub stopping: tokio::sync::Notify,
    pub succeeded: std::sync::atomic::AtomicBool,
    pub release: std::sync::atomic::AtomicBool,
    pub hold_delivery: bool,
    pub delivery: tokio::sync::Notify,
}
struct ReleaseWorker(Arc<Observer>);
impl Drop for ReleaseWorker {
    fn drop(&mut self) {
        self.0
            .release
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
async fn observed(signal: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(2), signal.notified())
        .await
        .expect("worker lifecycle boundary must be observed");
}
#[derive(Debug)]
struct Ownership(std::sync::atomic::AtomicBool);
impl otto_session::WriterOwnership for Ownership {
    fn check(&self) -> Result<(), String> {
        if self.0.load(std::sync::atomic::Ordering::Acquire) {
            Err("ownership lost".into())
        } else {
            Ok(())
        }
    }
    fn release(&self) -> Result<(), String> {
        self.0.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}
#[tokio::test]
async fn running_workers_exit_on_cancel_revoke_future_drop_and_ownership_loss() {
    for action in ["cancel", "revoke", "drop", "ownership"] {
        let fixture = Fixture::new();
        let path = package(&fixture, "while true do end");
        fixture.install(&path);
        fixture.service().change("lua-demo", "trust").unwrap();
        let runtime = runtime(&fixture);
        let observer = Arc::new(Observer::default());
        let release = ReleaseWorker(observer.clone());
        *runtime.lua_observer.lock().unwrap() = Some(observer.clone());
        let ownership = Arc::new(Ownership(std::sync::atomic::AtomicBool::new(false)));
        let owner = ownership.clone();
        let guard = session_guard(
            Arc::new(move || Some(owner.clone())),
            fixture.home.path().to_owned(),
            CancellationToken::new(),
            true,
        );
        let token = CancellationToken::new();
        let mut future = Box::pin(runtime.lua(
            "handler.lua",
            "10s",
            runtime.packages.first(),
            json!({}),
            &token,
            Some(&guard),
        ));
        tokio::select! {
            _ = observed(&observer.running) => {},
            _ = &mut future => panic!("must reach instruction hook before interruption"),
        }
        match action {
            "cancel" => token.cancel(),
            "revoke" => {
                fixture.service().change("lua-demo", "revoke").unwrap();
            }
            "ownership" => {
                ownership
                    .0
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            "drop" => {}
            _ => unreachable!(),
        }
        if action == "drop" {
            drop(future); // StopOnDrop, not just dropping an unpolled async body.
            drop(release);
            observed(&observer.exited).await;
        } else {
            tokio::select! {
                _ = observed(&observer.stopping) => {},
                _ = &mut future => panic!("host must join the still-running worker"),
            }
            drop(release);
            assert!(
                tokio::time::timeout(Duration::from_secs(2), future)
                    .await
                    .unwrap()
                    .is_err()
            );
            // Notify is after run returns, hence after VM destruction.
            observed(&observer.exited).await;
        }
    }
}
#[test]
fn queued_worker_uses_original_deadline_before_vm_admission() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            released.recv().unwrap();
        });
        entering.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(10);
        let observer = Arc::new(Observer::default());
        observer
            .release
            .store(true, std::sync::atomic::Ordering::Release);
        let worker_observer = observer.clone();
        let job = tokio::task::spawn_blocking(move || {
            super::lua_engine::run_observed(
                b"local x=0; for i=1,5000 do x=x+i end; return {}",
                b"{}",
                deadline,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                worker_observer,
            )
        });
        tokio::time::sleep_until(deadline.into()).await;
        release.send(()).unwrap();
        blocker.await.unwrap();
        let error = job.await.unwrap().unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), observer.running.notified())
                .await
                .is_err(),
            "expired queued worker must never run Lua instructions"
        );
    });
}
#[tokio::test]
async fn completed_success_first_polled_after_deadline_is_rejected() {
    let fixture = Fixture::new();
    let path = package(&fixture, "return {content='success'}");
    fixture.install(&path);
    fixture.service().change("lua-demo", "trust").unwrap();
    let runtime = runtime(&fixture);
    let observer = Arc::new(Observer {
        hold_delivery: true,
        ..Default::default()
    });
    *runtime.lua_observer.lock().unwrap() = Some(observer.clone());
    let token = CancellationToken::new();
    let mut future = Box::pin(runtime.lua(
        "handler.lua",
        "500ms",
        runtime.packages.first(),
        json!({}),
        &token,
        None,
    ));
    tokio::select! {
        _ = observed(&observer.exited) => {},
        _ = &mut future => panic!("delivery must wait while worker completes"),
    }
    assert!(
        observer
            .succeeded
            .load(std::sync::atomic::Ordering::Acquire),
        "worker must finish successfully inside its budget"
    );
    // Successful worker has finished, but caller has not polled its result.
    tokio::time::sleep(Duration::from_millis(510)).await;
    observer.delivery.notify_one();
    let error = future.await.unwrap_err();
    assert!(error.message.contains("timed out"), "{}", error.message);
    assert!(!token.is_cancelled());
}
