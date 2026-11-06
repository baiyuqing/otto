//! A `[memory] backend = "plugin:<package>"` source: the package's memory
//! command replaces the file store. Offline: a scripted provider and a shell
//! script package that logs its requests.
#[path = "memory_plugin_source/fixture.rs"]
mod fixture;

use otto::fixtures::scripted_provider;
use std::sync::{Arc, atomic::AtomicUsize};

use fixture::{Fixture, RECALL_MARKER, plugin_memory_config};
use otto::embed::{ApprovalDecision, Input};
use otto_core::agent::Event;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// One Otto instance may run per process: tests take this lock in turn.
static ONE_OTTO: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Startup composes a large future; give the test its own stack.
fn run<F: std::future::Future<Output = ()> + Send + 'static>(future: F) {
    let _turn = ONE_OTTO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| tokio::runtime::Runtime::new().unwrap().block_on(future))
        .unwrap()
        .join()
        .unwrap();
}

fn serve(replies: Vec<String>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    scripted_provider::serve(scripted_provider::Script {
        replies,
        served: Arc::new(AtomicUsize::new(0)),
    })
}

/// Runs one turn and returns the events' warning texts.
async fn turn(session: &otto::embed::Session, text: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    session
        .prompt(
            Input::Text(text),
            &mut |event| {
                if let Event::MemoryWarning { message } = event {
                    warnings.push(message);
                }
            },
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    warnings
}

fn request_json(bodies: &[String], index: usize) -> Value {
    serde_json::from_str(&bodies[index]).unwrap()
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(String::from))
        .collect()
}

fn message_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn recall_reaches_the_request_and_the_file_store_stays_closed() {
    run(async {
        let fixture = Fixture::new();
        fixture.install(true, false);
        let (url, requests) = serve(vec![scripted_provider::text_reply("done")]);
        let otto = fixture
            .builder(&url, &plugin_memory_config())
            .build()
            .await
            .unwrap();
        let session = otto.new_session(None).await.unwrap();
        let warnings = turn(&session, "what do you remember").await;
        assert!(warnings.is_empty(), "{warnings:?}");
        let bodies = requests.lock().unwrap().clone();
        let request = request_json(&bodies, 0);
        assert!(message_text(&request).contains(RECALL_MARKER));
        assert!(tool_names(&request).contains(&"memory_search".to_string()));
        let operations = fixture.operations();
        assert_eq!(operations.first().map(String::as_str), Some("describe"));
        assert!(operations.contains(&"recall".to_string()), "{operations:?}");
        assert!(!fixture.file_store().exists());
        session.close().await.unwrap();
        otto.close().await.unwrap();
    });
}

#[test]
fn a_failing_recall_command_is_a_warning_and_the_turn_completes() {
    run(async {
        let fixture = Fixture::new();
        fixture.install(true, true);
        let (url, _) = serve(vec![scripted_provider::text_reply("done")]);
        let otto = fixture
            .builder(&url, &plugin_memory_config())
            .build()
            .await
            .unwrap();
        let session = otto.new_session(None).await.unwrap();
        let warnings = turn(&session, "hello").await;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        session.close().await.unwrap();
        otto.close().await.unwrap();
    });
}

#[test]
fn disabled_memory_never_runs_the_package_command() {
    run(async {
        let fixture = Fixture::new();
        fixture.install(true, false);
        let (url, requests) = serve(vec![scripted_provider::text_reply("done")]);
        let memory = format!("{}\nenabled = false", plugin_memory_config());
        let otto = fixture.builder(&url, &memory).build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        turn(&session, "hello").await;
        let bodies = requests.lock().unwrap().clone();
        assert!(!tool_names(&request_json(&bodies, 0)).contains(&"memory_search".to_string()));
        assert!(fixture.operations().is_empty());
        assert!(!fixture.file_store().exists());
        session.close().await.unwrap();
        otto.close().await.unwrap();
    });
}

#[test]
fn a_finished_turn_is_observed_with_text_tool_call_and_tool_result() {
    run(async {
        let fixture = Fixture::new();
        fixture.install(true, false);
        std::fs::create_dir_all(fixture.workspace()).unwrap();
        std::fs::write(fixture.workspace().join("notes.txt"), "file-body-marker").unwrap();
        let (url, _) = serve(vec![
            scripted_provider::tool_call_reply("call-1", "read", r#"{"path":"notes.txt"}"#),
            scripted_provider::text_reply("assistant-final-text"),
        ]);
        let otto = fixture
            .builder(&url, &plugin_memory_config())
            .build()
            .await
            .unwrap();
        let session = otto.new_session(None).await.unwrap();
        turn(&session, "user-turn-text").await;
        let observe: Vec<Value> = fixture
            .requests()
            .into_iter()
            .filter(|request| request["operation"] == "observe")
            .collect();
        assert_eq!(observe.len(), 1, "{observe:?}");
        let observations = observe[0]["request"]["observations"].as_array().unwrap();
        let has = |role: &str, needle: &str| {
            observations.iter().any(|observation| {
                observation["source"]["role"] == role
                    && observation["content"].to_string().contains(needle)
            })
        };
        assert!(has("user", "user-turn-text"), "{observations:?}");
        assert!(has("tool_call", "call-1"), "{observations:?}");
        assert!(has("tool_result", "file-body-marker"), "{observations:?}");
        assert!(has("assistant", "assistant-final-text"), "{observations:?}");
        assert!(!fixture.file_store().exists());
        session.close().await.unwrap();
        otto.close().await.unwrap();
    });
}

#[test]
fn an_untrusted_package_fails_session_creation_when_required() {
    run(async {
        let fixture = Fixture::new();
        fixture.install(false, false);
        let (url, _) = serve(vec![]);
        let memory = format!("{}\nrequired = true", plugin_memory_config());
        let otto = fixture.builder(&url, &memory).build().await;
        let error = match otto {
            Ok(otto) => {
                let created = otto.new_session(None).await;
                otto.close().await.unwrap();
                match created {
                    Ok(_) => panic!("session creation must fail"),
                    Err(error) => format!("{error} {error:?}"),
                }
            }
            Err(error) => format!("{error} {error:?}"),
        };
        assert!(error.contains("not trusted"), "{error}");
        assert!(fixture.operations().is_empty());
    });
}

// The runner build writes its warning to the process stderr, which an
// embedded host cannot capture; the warning text is covered by the unit test
// in `runtime/wiring/plugin_memory_tests.rs`.
#[test]
fn an_untrusted_package_gives_no_memory_tools_when_not_required() {
    run(async {
        let fixture = Fixture::new();
        fixture.install(false, false);
        let (url, requests) = serve(vec![scripted_provider::text_reply("done")]);
        let otto = fixture
            .builder(&url, &plugin_memory_config())
            .build()
            .await
            .unwrap();
        let session = otto.new_session(None).await.unwrap();
        assert!(turn(&session, "hello").await.is_empty());
        let bodies = requests.lock().unwrap().clone();
        assert!(!tool_names(&request_json(&bodies, 0)).contains(&"memory_search".to_string()));
        assert!(fixture.operations().is_empty());
        assert!(!fixture.file_store().exists());
        session.close().await.unwrap();
        otto.close().await.unwrap();
    });
}

struct InjectedService;

#[async_trait::async_trait]
impl otto_core::memory::MemoryService for InjectedService {
    fn describe(&self) -> otto_core::memory::ServiceDescription {
        let mut description = otto_core::memory::ServiceDescription::new("injected".into());
        description
            .capabilities
            .push(otto_core::memory::Capability::new(
                otto_core::memory::Operation::Recall,
            ));
        description
    }
    async fn recall(
        &self,
        _: &otto_core::memory::RecallRequest,
        _: &dyn otto_core::operation::OperationControl,
    ) -> otto_core::memory::Result<otto_core::memory::RecallResult> {
        Err(otto_core::memory::Error::new(
            otto_core::memory::ErrorKind::Unsupported,
        ))
    }
}

#[test]
fn an_injected_extension_with_a_plugin_backend_fails_startup() {
    run(async {
        let fixture = Fixture::new();
        fixture.install(true, false);
        let (url, _) = serve(vec![]);
        let result = fixture
            .builder(&url, &plugin_memory_config())
            .memory_service(otto::runtime::wiring::MemoryExtension::owned(
                Arc::new(InjectedService),
                otto_core::memory::Scope {
                    namespace: "user".into(),
                    id: "u1".into(),
                },
            ))
            .build()
            .await;
        let error = match result {
            Ok(_) => panic!("startup must fail"),
            Err(error) => format!("{error} {error:?}"),
        };
        assert!(
            error.contains("cannot be combined with backend = \"plugin:memfix\""),
            "{error}"
        );
        assert!(fixture.operations().is_empty());
    });
}
