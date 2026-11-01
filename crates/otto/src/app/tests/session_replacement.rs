//! New, resume, model switch, rename and archive tests.

use super::*;

#[tokio::test]
async fn info_carries_the_context_window_and_the_usage_snapshot() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let info = controller.info();
    assert!(!info.usage_present);
    assert_eq!(info.usage, Usage::default());
    assert_eq!(info.context_input_tokens, 0);
    assert!(!info.context_input_tokens_present);
}

#[tokio::test]
async fn new_session_replaces_the_session_and_keeps_the_runtime() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let before = controller.info();

    controller.new_session().await.expect("new session");

    let after = controller.info();
    assert_ne!(after.session_id, before.session_id);
    assert_eq!(after.model, "gpt-alpha");
    assert_eq!(after.model_selector, "alpha/gpt-alpha");
}

#[tokio::test]
async fn new_session_keeps_runtime_changed_in_the_current_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    controller
        .switch_model("beta/gpt-beta")
        .await
        .expect("switch");
    controller.set_thinking("high").await.expect("thinking");
    let before = controller.info();

    controller.new_session().await.expect("new session");

    let after = controller.info();
    assert_ne!(after.session_id, before.session_id);
    assert_eq!(after.model_selector, "beta/gpt-beta");
    assert_eq!(after.model, "gpt-beta");
    assert_eq!(after.thinking, "high");
}

#[tokio::test]
async fn resume_keeps_process_selection_and_appends_without_rewriting_history() {
    use crate::runtime::builder::TEST_PROVIDERS;
    use crate::subagent::doubles_tests::{FakeProvider, assistant_text, match_any};
    struct ProviderGuard(String);
    impl Drop for ProviderGuard {
        fn drop(&mut self) {
            TEST_PROVIDERS
                .lock()
                .unwrap()
                .retain(|(home, _)| home != &self.0);
        }
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let home = workspace.path().to_string_lossy().into_owned();
    TEST_PROVIDERS
        .lock()
        .unwrap()
        .push((home.clone(), provider.clone()));
    let _provider_guard = ProviderGuard(home);
    let controller = controller(workspace.path(), sessions.path()).await;
    controller.set_thinking("low").await.unwrap();
    controller
        .prompt("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    let first = controller.info();
    controller.new_session().await.unwrap();
    controller.switch_model("beta/gpt-beta").await.unwrap();
    controller.set_thinking("high").await.unwrap();
    let selected = controller.info();
    let original = std::fs::read(&first.session_path).unwrap();

    controller
        .resume_session(&first.session_path)
        .await
        .unwrap();

    let resumed = controller.info();
    assert_eq!(resumed.session_id, first.session_id);
    assert_eq!(resumed.model_selector, selected.model_selector);
    assert_eq!(resumed.provider, selected.provider);
    assert_eq!(resumed.provider_type, selected.provider_type);
    assert_eq!(resumed.model, selected.model);
    assert_eq!(resumed.thinking, selected.thinking);
    assert_eq!(controller.history().len(), 2);
    let appended = std::fs::read(&first.session_path).unwrap();
    assert!(
        appended.starts_with(&original),
        "history must remain append-only"
    );
    assert!(
        appended.len() > original.len(),
        "active provenance is appended"
    );
    controller
        .prompt("continue", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(provider.requests().last().unwrap().model, selected.model);
    let records = std::fs::read_to_string(&first.session_path).unwrap();
    let records: Vec<serde_json::Value> = records
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let assistant = records
        .iter()
        .rev()
        .find(|record| record["message"]["role"] == "assistant")
        .expect("persisted assistant response");
    assert_eq!(assistant["message"]["model"], selected.model);
    assert_eq!(assistant["message"]["provider"], selected.provider_type);
    let historical = records
        .iter()
        .find(|record| record["message"]["role"] == "assistant")
        .unwrap();
    assert_eq!(historical["message"]["model"], first.model);
    assert_eq!(historical["message"]["provider"], first.provider_type);
    let requests = provider.requests();
    assert_eq!(requests[0].model, first.model);
    assert_eq!(requests[0].thinking, "low");
    assert_eq!(requests[1].thinking, "high");
    controller.set_thinking("unset").await.unwrap();
    controller.new_session().await.unwrap();
    assert_eq!(controller.info().model, selected.model);
    assert_eq!(controller.info().thinking, "");
    controller
        .resume_session(&first.session_path)
        .await
        .unwrap();
    assert_eq!(controller.info().model, selected.model);
    assert_eq!(controller.info().thinking, "");
}

#[tokio::test]
async fn opening_a_session_uses_config_instead_of_historical_selection() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");

    controller
        .switch_model("beta/gpt-beta")
        .await
        .expect("switch");
    controller.set_thinking("high").await.expect("thinking");
    let path = controller.info().session_path;
    assert!(!path.is_empty());
    controller.close().expect("close");

    let builder = builder(workspace.path(), sessions.path());
    let (reopened, warnings) = Controller::open(Arc::new(builder), Path::new(&path))
        .await
        .expect("open");

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reopened.info().model_selector, "alpha/gpt-alpha");
    assert_eq!(reopened.info().model, "gpt-alpha");
    assert_eq!(reopened.info().thinking, "");
}

#[tokio::test]
async fn switching_model_switches_provider_and_model() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    assert_eq!(
        controller.models(),
        vec!["alpha/gpt-alpha", "beta/gpt-beta"]
    );

    let before = controller.info();

    controller
        .switch_model("beta/gpt-beta")
        .await
        .expect("switch");

    let info = controller.info();
    assert_eq!(info.session_id, before.session_id);
    assert_eq!(info.model_selector, "beta/gpt-beta");
    assert_eq!(info.provider, "beta");
    assert_eq!(info.model, "gpt-beta");
}

#[tokio::test]
async fn an_unknown_provider_leaves_the_session_in_place() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let before = controller.info();

    let error = controller
        .switch_model("missing/gpt-x")
        .await
        .expect_err("unknown provider");
    assert!(error.contains("unknown provider"), "{error}");
    assert_eq!(controller.info().session_id, before.session_id);
}

#[tokio::test]
async fn renaming_records_the_name_on_the_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    controller
        .rename_session("  release notes  ")
        .expect("rename");

    assert_eq!(controller.info().session_name, "release notes");
    assert_eq!(
        controller.rename_session("   ").expect_err("blank"),
        INVALID_SESSION_NAME
    );
}

#[tokio::test]
async fn archiving_moves_the_file_and_starts_a_fresh_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    // The store is lazy: one appended message gives the session a file.
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let before = controller.info();
    assert!(!before.session_path.is_empty());

    let result = controller.archive_current_session().await.expect("archive");

    assert!(result.path.contains("archive"), "{}", result.path);
    assert!(Path::new(&result.path).exists());
    assert!(!Path::new(&before.session_path).exists());
    assert_ne!(controller.info().session_id, before.session_id);
}

#[tokio::test]
async fn archiving_cancels_the_session_timers() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let reminders = controller.reminders().expect("timer registry");
    reminders
        .schedule(std::time::Duration::from_secs(60), "outstanding".into())
        .expect("schedule");

    controller.archive_current_session().await.expect("archive");

    assert!(
        reminders.list().is_empty(),
        "archiving must cancel outstanding timers"
    );
    assert!(controller.reminders().expect("registry").list().is_empty());
}

#[tokio::test]
async fn a_session_resumed_from_the_archive_keeps_its_timers_beside_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let archived = controller.archive_current_session().await.expect("archive");
    // The same open sequence as `--resume PATH` in `cli::run`.
    let builder = builder(workspace.path(), sessions.path());
    let runtime = initial_runtime(&builder);
    let (store, _) = crate::session::Prepared::prepare(Path::new(&archived.path), None)
        .expect("prepare archived")
        .activate()
        .expect("activate archived");
    let session = SharedSession::new(Arc::new(store));
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let reopened = Controller::new(builder, true, session, runner, info);

    reopened
        .reminders()
        .expect("timer registry")
        .schedule(std::time::Duration::from_secs(60), "outstanding".into())
        .expect("schedule");

    assert!(
        Path::new(&archived.path)
            .with_extension("reminders.json")
            .exists(),
        "the timer sidecar is written beside the archived session file"
    );
}

#[tokio::test]
async fn a_session_with_no_file_cannot_be_archived() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let error = controller
        .archive_current_session()
        .await
        .expect_err("no file");
    assert_eq!(error, PERSISTENCE_DISABLED);
}

#[tokio::test]
async fn the_default_model_is_written_to_the_configuration_file() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    std::fs::write(
        controller.config_path(),
        concat!(
            "default_model = \"alpha/gpt-alpha\"\n\n[providers.alpha]\ntype = \"openai-compatible\"\n",
            "base_url = \"https://a.example/v1\"\n\n[providers.beta]\ntype = \"openai-compatible\"\n",
            "base_url = \"https://b.example/v1\"\n"
        ),
    )
    .expect("write config");

    controller
        .set_default_model("beta/gpt-beta")
        .expect("set default");

    let content = std::fs::read_to_string(controller.config_path()).expect("read config");
    assert!(
        content.contains("default_model = \"beta/gpt-beta\""),
        "{content}"
    );
}
