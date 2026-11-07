use otto_core::model::EffectCertainty;

use super::*;
use crate::schedule::Store;
use crate::tool::testutil::{run, run_cancelled};

fn context(origin: Option<Origin>) -> Context {
    let turn = TurnOrigin::default();
    turn.set(origin);
    Context {
        store: Some(Arc::new(Store::open_in_memory().unwrap())),
        origin: Arc::new(turn),
        session_id: "sess".into(),
        workspace: "/work".into(),
    }
}

fn telegram() -> Origin {
    Origin::chat("telegram", "42").unwrap()
}

fn by_name(context: &Context, name: &str) -> Box<dyn Tool + Send + Sync> {
    tools(context)
        .into_iter()
        .find(|tool| tool.definition().name == name)
        .unwrap()
}

async fn call(context: &Context, name: &str, arguments: &str) -> ToolResult {
    run(by_name(context, name).as_ref(), arguments).await
}

#[test]
fn definitions_match_names() {
    let names: Vec<_> = definitions().into_iter().map(|d| d.name).collect();
    assert_eq!(names, NAMES);
    let schedule = &definitions()[0];
    assert!(schedule.description.contains("`remind` is your own timer"));
    // A caller that sets every property needs a non-repeating `repeat`.
    let parameters: serde_json::Value =
        serde_json::from_str(schedule.parameters.as_ref().unwrap().get()).unwrap();
    assert_eq!(parameters["properties"]["repeat"]["enum"][0], "once");
}

#[tokio::test]
async fn schedule_reports_the_resolved_time_target_and_now() {
    let context = context(Some(telegram()));
    let result = call(
        &context,
        "schedule",
        r#"{"text":"stand up","seconds":3600}"#,
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    let lines: Vec<_> = result.content.lines().collect();
    assert_eq!(lines.len(), 2, "{}", result.content);
    assert!(
        lines[0].starts_with("scheduled s1: text, delivery 20"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("(in 1h0m); delivered to this Telegram chat"),
        "{}",
        lines[0]
    );
    assert!(lines[1].starts_with("now: 20"), "{}", lines[1]);
}

fn spec(arguments: &str) -> Option<ApprovalSpec> {
    let context = context(Some(telegram()));
    by_name(&context, "schedule")
        .approval_spec(&crate::tool::testutil::raw(arguments))
        .unwrap()
}

#[test]
fn prompt_items_need_approval_and_text_items_do_not() {
    for (arguments, title, repeat) in [
        (
            r#"{"prompt":"check the build","seconds":60}"#,
            "Schedule an unattended run?",
            "once",
        ),
        (
            r#"{"prompt":"check the build","at":"08:00","repeat":"daily","text":""}"#,
            "Schedule a daily unattended run?",
            "daily",
        ),
    ] {
        let spec = spec(arguments).expect(arguments);
        assert_eq!(spec.source, "schedule");
        assert_eq!(spec.title, title);
        assert_eq!(spec.version, crate::approval::digest(arguments));
        assert!(!spec.persistent);
        let details: serde_json::Value = serde_json::from_str(&spec.details).unwrap();
        assert_eq!(details["prompt"], "check the build");
        assert_eq!(details["repeat"], repeat);
    }
    assert!(spec(r#"{"text":"stand up","seconds":60}"#).is_none());
    assert!(spec(r#"{"text":"stand up","prompt":"","seconds":60}"#).is_none());
}

#[tokio::test]
async fn requests_without_an_approval_spec_never_create_a_prompt_item() {
    let context = context(Some(telegram()));
    for arguments in [
        r#"{"text":"a","prompt":"b","seconds":60}"#,
        r#"{"seconds":60}"#,
        r#"{"prompt":"b","seconds":60,"extra":1}"#,
        r#"[1]"#,
    ] {
        assert!(spec(arguments).is_none(), "{arguments}");
        let result = call(&context, "schedule", arguments).await;
        assert!(result.is_error, "{arguments}: {result:?}");
    }
    let listing = call(&context, "schedule_list", "{}").await;
    assert!(
        listing.content.contains("no scheduled items"),
        "{listing:?}"
    );
}

#[tokio::test]
async fn schedule_treats_blank_fields_as_absent() {
    let cases = [
        (
            r#"{"at":"","prompt":"weather in Beijing","repeat":"once","seconds":120,"text":""}"#,
            Kind::Prompt,
            "weather in Beijing",
            None,
        ),
        (
            r#"{"text":"stand up","prompt":"  ","at":"08:00","repeat":"daily"}"#,
            Kind::Text,
            "stand up",
            Some(Repeat::Daily),
        ),
        (
            r#"{"text":"a","seconds":5,"repeat":""}"#,
            Kind::Text,
            "a",
            None,
        ),
    ];
    for (arguments, kind, body, repeat) in cases {
        let context = context(Some(telegram()));
        let result = call(&context, "schedule", arguments).await;
        assert!(!result.is_error, "{arguments} => {result:?}");
        let items = context
            .store
            .as_ref()
            .unwrap()
            .list(&telegram(), Utc::now())
            .unwrap();
        assert_eq!(items.len(), 1, "{arguments}");
        assert_eq!(items[0].kind, kind, "{arguments}");
        assert_eq!(items[0].body, body, "{arguments}");
        assert_eq!(items[0].repeat, repeat, "{arguments}");
    }
}

#[tokio::test]
async fn schedule_repeat_names_the_rule_and_targets() {
    let cases = [
        (
            Origin::terminal(),
            "any running otto terminal (TUI or REPL)",
        ),
        (Origin::web(), "the otto web UI"),
        (
            Origin::chat("telegram", "c1").unwrap(),
            "this Telegram chat",
        ),
    ];
    for (origin, target) in cases {
        let context = context(Some(origin));
        let result = call(
            &context,
            "schedule",
            r#"{"prompt":"weather in Beijing","at":"08:00","repeat":"weekdays"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        let first = result.content.lines().next().unwrap();
        assert!(
            first.starts_with("scheduled s1: prompt, first run "),
            "{first}"
        );
        assert!(
            first.contains(", repeats weekdays; delivered to "),
            "{first}"
        );
        assert!(first.ends_with(target), "{first}");
        let stored = context
            .store
            .as_ref()
            .unwrap()
            .list(&context.origin.get().unwrap(), Utc::now())
            .unwrap();
        assert_eq!(stored[0].repeat, Some(Repeat::Weekdays));
        assert_eq!(stored[0].workspace, "/work");
        assert_eq!(stored[0].session_id, "sess");
    }
}

#[tokio::test]
async fn invalid_arguments_are_rejected_before_anything_is_stored() {
    let context = context(Some(telegram()));
    let long_text = format!(r#"{{"text":"{}","seconds":5}}"#, "x".repeat(501));
    let long_prompt = format!(r#"{{"prompt":"{}","seconds":5}}"#, "x".repeat(2001));
    let cases = [
        (
            r#"{"seconds":5}"#.to_string(),
            "exactly one of text and prompt",
        ),
        (
            r#"{"text":"a","prompt":"b","seconds":5}"#.into(),
            "exactly one of text and prompt",
        ),
        (r#"{"text":"a"}"#.into(), "exactly one of seconds and at"),
        (
            r#"{"text":"a","seconds":5,"at":"08:00"}"#.into(),
            "exactly one of seconds and at",
        ),
        (
            r#"{"text":"  ","seconds":5}"#.into(),
            "exactly one of text and prompt",
        ),
        (
            r#"{"text":"","prompt":"","seconds":5}"#.into(),
            "exactly one of text and prompt",
        ),
        (
            r#"{"at":"","prompt":"x","repeat":"daily","seconds":120,"text":""}"#.into(),
            "\"once\"",
        ),
        (long_text, "text must be 1 to 500"),
        (long_prompt, "prompt must be 1 to 2000"),
        (
            r#"{"text":"a","seconds":0}"#.into(),
            "seconds must be between",
        ),
        (
            r#"{"text":"a","seconds":2592001}"#.into(),
            "seconds must be between",
        ),
        (
            r#"{"text":"a","seconds":5,"repeat":"daily"}"#.into(),
            "repeat needs `at`",
        ),
        (
            r#"{"text":"a","at":"2026-12-31 08:00","repeat":"daily"}"#.into(),
            "at must be",
        ),
        (
            r#"{"text":"a","at":"08:00","repeat":"hourly"}"#.into(),
            "repeat must be",
        ),
        (
            r#"{"text":"a","at":"2020-01-01 08:00"}"#.into(),
            "after the current time",
        ),
        (r#"{"text":"a","at":"soon"}"#.into(), "at must be HH:MM"),
        (
            r#"{"text":"a","seconds":5,"cron":"* * * * *"}"#.into(),
            "unknown field",
        ),
    ];
    for (arguments, snippet) in cases {
        let result = call(&context, "schedule", &arguments).await;
        assert!(result.is_error, "{arguments} => {result:?}");
        assert!(
            result.content.contains(snippet),
            "{arguments} => {}",
            result.content
        );
        let outcome = result.outcome_override.as_ref().expect("typed outcome");
        assert_eq!(
            outcome.effect_certainty,
            EffectCertainty::NotStarted,
            "{arguments}"
        );
    }
    let items = context
        .store
        .as_ref()
        .unwrap()
        .list(&telegram(), Utc::now())
        .unwrap();
    assert!(items.is_empty());
}

#[tokio::test]
async fn all_tools_report_a_missing_origin_or_store() {
    let no_origin = context(None);
    let no_store = Context {
        store: None,
        ..context(Some(telegram()))
    };
    let calls = [
        ("schedule", r#"{"text":"a","seconds":5}"#),
        ("schedule_list", "{}"),
        ("schedule_cancel", r#"{"id":"s1"}"#),
    ];
    for (name, arguments) in calls {
        let result = call(&no_origin, name, arguments).await;
        assert!(
            result.is_error
                && result
                    .content
                    .contains("no chat, terminal or web UI origin"),
            "{name}: {result:?}"
        );
        let result = call(&no_store, name, arguments).await;
        assert!(
            result.is_error
                && result
                    .content
                    .contains("~/.otto/schedule.db could not be opened"),
            "{name}: {result:?}"
        );
    }
}

#[tokio::test]
async fn the_origin_is_read_at_call_time() {
    let context = context(None);
    let arguments = r#"{"text":"a","seconds":5}"#;
    assert!(call(&context, "schedule", arguments).await.is_error);
    context.origin.set(Some(Origin::web()));
    assert!(!call(&context, "schedule", arguments).await.is_error);
}

#[tokio::test]
async fn list_and_cancel_are_limited_to_the_current_origin() {
    let context = context(Some(telegram()));
    call(&context, "schedule", r#"{"text":"mine","seconds":60}"#).await;
    context.origin.set(Some(Origin::web()));
    call(
        &context,
        "schedule",
        r#"{"prompt":"theirs\nsecond line","seconds":60}"#,
    )
    .await;

    let listing = call(&context, "schedule_list", "{}").await.content;
    let lines: Vec<_> = listing.lines().collect();
    assert!(lines[0].starts_with("now: 20"), "{listing}");
    assert_eq!(lines.len(), 2, "{listing}");
    assert!(lines[1].starts_with("s2 prompt once next 20"), "{listing}");
    assert!(lines[1].ends_with(": theirs second line"), "{listing}");

    let denied = call(&context, "schedule_cancel", r#"{"id":"s1"}"#).await;
    assert!(
        denied.is_error && denied.content.contains("no active scheduled item s1"),
        "{denied:?}"
    );
    let cancelled = call(&context, "schedule_cancel", r#"{"id":"s2"}"#).await;
    assert_eq!(cancelled.content, "cancelled s2: theirs second line");
    let listing = call(&context, "schedule_list", "{}").await.content;
    assert!(listing.contains("s2 prompt once cancelled 20"), "{listing}");

    context.origin.set(Some(telegram()));
    let listing = call(&context, "schedule_list", "{}").await.content;
    assert!(
        listing.contains("s1 text once next 20") && !listing.contains("s2"),
        "{listing}"
    );
    for bad in [r#"{"id":"7"}"#, r#"{"id":"s0"}"#, r#"{"id":"sx"}"#, "{}"] {
        assert!(
            call(&context, "schedule_cancel", bad).await.is_error,
            "{bad}"
        );
    }
}

#[tokio::test]
async fn an_empty_list_and_a_failed_item_are_described() {
    let context = context(Some(telegram()));
    let empty = call(&context, "schedule_list", "{}").await.content;
    assert!(empty.ends_with("\nno scheduled items"), "{empty}");
    assert!(call(&context, "schedule_list", r#"{"x":1}"#).await.is_error);

    let store = context.store.clone().unwrap();
    let now = Utc::now();
    let created = store
        .create(
            NewItem {
                origin: telegram(),
                session_id: "s".into(),
                workspace: "/w".into(),
                kind: Kind::Text,
                body: "old".into(),
                repeat: None,
                local_time: None,
                due_at: now - chrono::TimeDelta::hours(30),
            },
            now,
        )
        .unwrap();
    store.sweep(now).unwrap();
    let listing = call(&context, "schedule_list", "{}").await.content;
    assert!(
        listing.contains(&format!("s{} text once failed", created.id)),
        "{listing}"
    );
    assert!(
        listing.contains("[last error: not delivered within 24 h of the due time]"),
        "{listing}"
    );
}

#[tokio::test]
async fn a_cancelled_token_starts_nothing() {
    let context = context(Some(telegram()));
    let tool = by_name(&context, "schedule");
    let result = run_cancelled(tool.as_ref(), r#"{"text":"a","seconds":5}"#).await;
    assert!(result.is_error);
    assert!(
        context
            .store
            .as_ref()
            .unwrap()
            .list(&telegram(), Utc::now())
            .unwrap()
            .is_empty()
    );
}
