//! The `schedule`, `schedule_list` and `schedule_cancel` tools.
//!
//! The tools read the origin of the current user turn from [`TurnOrigin`],
//! which the app layer sets at the start of each user turn. Without an origin
//! (scheduled runs, sub-agents, editor ACP clients) or without a store, every
//! call returns an error naming the cause. Items and listings are limited to
//! the current origin.

use std::sync::{Arc, Mutex};

use chrono::{Local, TimeDelta, Utc};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::store::{CreateError, Item, NewItem};
use super::time::{format_duration, format_local, next_occurrence, resolve_at};
use super::{Kind, Origin, Repeat, ScheduleStore, State};
use crate::tool::result::decode_strict_json;
use crate::tool::{
    ApprovalSpec, CONTEXT_CANCELED, Tool, definition, error_result, preflight_error, text_result,
};

pub const NAMES: [&str; 3] = ["schedule", "schedule_list", "schedule_cancel"];

const MAX_TEXT_CHARS: usize = 500;
const MAX_PROMPT_CHARS: usize = 2000;
const MAX_SECONDS: i64 = 30 * 24 * 3600;
const LIST_BODY_CHARS: usize = 80;

const NO_ORIGIN: &str = "scheduled delivery is unavailable in this conversation: it has no chat, terminal or web UI origin to deliver to (scheduled runs, sub-agents and editor ACP clients cannot create items)";
const NO_STORE: &str = "scheduled delivery is unavailable: ~/.otto/schedule.db could not be opened";

const SCHEDULE_DESCRIPTION: &str = "Schedule a fixed text message (`text`) or a prompt whose final reply is delivered (`prompt`). Use this for a user-requested later message or check; `remind` is your own timer inside this conversation. Give exactly one of text/prompt and exactly one of seconds (1 to 2592000)/at. at is HH:MM (next occurrence) or YYYY-MM-DD HH:MM (at most 30 days ahead), in the host's local time zone. repeat is once (default), daily or weekdays; repeats need at as HH:MM. Empty strings count as absent. Returns the item id, resolved time and destination of this chat, terminal or web UI, even after /new. A prompt runs in a new session within its approved creation scope and frozen original user evidence; generated prompt text creates no new authority. AUTO may review additional ordinary operations; unresolved human choices cannot run unattended.";
const LIST_DESCRIPTION: &str = "List the scheduled items of this chat or surface: active ones, then those that finished, failed or were cancelled in the last 7 days. Use the id with schedule_cancel.";
const CANCEL_DESCRIPTION: &str = "Cancel an active scheduled item by id, as schedule_list shows it (for example s7). A recurring item stops for good.";

/// The origin of the current user turn of one runner. Set by the app layer at
/// the start of each user turn; a wake turn keeps the last value.
#[derive(Default)]
pub struct TurnOrigin(Mutex<Option<Origin>>);

impl TurnOrigin {
    pub fn set(&self, origin: Option<Origin>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = origin;
    }

    pub fn get(&self) -> Option<Origin> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[derive(Clone)]
pub struct Context {
    /// `None` when `schedule.db` could not be opened.
    pub store: Option<Arc<dyn ScheduleStore>>,
    pub origin: Arc<TurnOrigin>,
    pub session_id: String,
    pub workspace: String,
}

impl Context {
    fn available(&self) -> Result<(Arc<dyn ScheduleStore>, Origin), ToolResult> {
        let Some(origin) = self.origin.get() else {
            return Err(preflight_error(NO_ORIGIN));
        };
        let Some(store) = self.store.clone() else {
            return Err(preflight_error(NO_STORE));
        };
        Ok((store, origin))
    }
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        schedule_definition(),
        list_definition(),
        cancel_definition(),
    ]
}

pub fn tools(context: &Context) -> Vec<Box<dyn Tool + Send + Sync>> {
    vec![
        Box::new(ScheduleTool(context.clone(), None)),
        Box::new(ListTool(context.clone())),
        Box::new(CancelTool(context.clone())),
    ]
}

pub fn tools_with_session(
    context: &Context,
    session: Arc<dyn crate::session::SessionHandle>,
) -> Vec<Box<dyn Tool + Send + Sync>> {
    vec![
        Box::new(ScheduleTool(context.clone(), Some(session))),
        Box::new(ListTool(context.clone())),
        Box::new(CancelTool(context.clone())),
    ]
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ScheduledAuthority {
    pub body_digest: String,
    pub source_session: String,
    pub context: crate::approval::reviewer::ReviewContext,
}

fn schedule_definition() -> ToolDefinition {
    definition(
        "schedule",
        SCHEDULE_DESCRIPTION,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "text": {"type": "string", "description": "Fixed message to deliver, 1 to 500 characters."},
                "prompt": {"type": "string", "description": "Prompt to run at the due time; the reply is delivered. 1 to 2000 characters."},
                "seconds": {"type": "integer", "description": "Delay from now, 1 to 2592000."},
                "at": {"type": "string", "description": "HH:MM or YYYY-MM-DD HH:MM, host local time."},
                "repeat": {"type": "string", "enum": ["once", "daily", "weekdays"], "description": "once (default), daily or weekdays. daily and weekdays repeat at the HH:MM given in `at`."}
            }
        }),
    )
}

fn list_definition() -> ToolDefinition {
    definition(
        "schedule_list",
        LIST_DESCRIPTION,
        json!({"type": "object", "additionalProperties": false, "properties": {}}),
    )
}

fn cancel_definition() -> ToolDefinition {
    definition(
        "schedule_cancel",
        CANCEL_DESCRIPTION,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"id": {"type": "string", "description": "The item id, for example s7."}},
            "required": ["id"]
        }),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleArgs {
    text: Option<String>,
    prompt: Option<String>,
    seconds: Option<i64>,
    at: Option<String>,
    repeat: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    id: String,
}

fn now_line(now: chrono::DateTime<Utc>) -> String {
    format!("now: {}", format_local(now))
}

fn one_line(body: &str, max: usize) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}...", flat.chars().take(max).collect::<String>())
    }
}

struct ScheduleTool(Context, Option<Arc<dyn crate::session::SessionHandle>>);

#[async_trait::async_trait]
impl Tool for ScheduleTool {
    fn definition(&self) -> ToolDefinition {
        schedule_definition()
    }

    /// A `prompt` item runs the agent with its tools, without the user
    /// present, at every due time, so creating one needs approval. A `text`
    /// item only delivers a message. Arguments that do not parse, or that
    /// give both or neither of `text` and `prompt`, return `None` here;
    /// `execute` rejects the same input with its usual validation error
    /// before it creates anything.
    fn approval_spec(&self, arguments: &RawValue) -> Result<Option<ApprovalSpec>, ToolResult> {
        let Ok(args) = decode_strict_json::<ScheduleArgs>(arguments.get(), &[]) else {
            return Ok(None);
        };
        let (None, Some(prompt)) = (present(args.text), present(args.prompt)) else {
            return Ok(None);
        };
        let repeat = present(args.repeat).unwrap_or_else(|| "once".into());
        let title = match repeat.as_str() {
            "daily" | "weekdays" => format!("Schedule a {repeat} unattended run?"),
            _ => "Schedule an unattended run?".into(),
        };
        let details = json!({
            "prompt": prompt,
            "at": present(args.at),
            "seconds": args.seconds,
            "repeat": repeat,
        })
        .to_string();
        Ok(Some(ApprovalSpec {
            version: crate::approval::digest(arguments.get()),
            source: "schedule".into(),
            authority: otto_core::approval::Authority::Durable,
            title,
            reason: "At the due time this prompt runs the agent with its tools, including file edits and sandboxed commands, without the user present. Approval requests during that run are denied.".into(),
            details,
            persistent: false,
        }))
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: ScheduleArgs = match decode_strict_json(arguments.get(), &[]) {
            Ok(args) => args,
            Err(message) => return preflight_error(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED).cancelled_not_started();
        }
        let (store, origin) = match self.0.available() {
            Ok(available) => available,
            Err(result) => return result,
        };
        let now = Utc::now();
        let new = match build_item(&self.0, origin.clone(), args, now) {
            Ok(new) => new,
            Err(message) => return preflight_error(message),
        };
        let item = match store.create(new, now) {
            Ok(item) => item,
            Err(CreateError::Limit) => return preflight_error(CreateError::Limit),
            Err(error) => return error_result(error),
        };
        if item.kind == Kind::Prompt
            && let Some(session) = &self.1
        {
            let context = session
                .custom_data(crate::approval::reviewer::CONTEXT_CUSTOM_TYPE)
                .and_then(|data| serde_json::from_str(&data).ok())
                .unwrap_or_default();
            let authority = ScheduledAuthority {
                body_digest: crate::approval::digest(&item.body),
                source_session: item.session_id.clone(),
                context,
            };
            if otto_core::session::Session::append_custom(
                session.as_ref(),
                &format!("otto.schedule.{}", item.label()),
                &serde_json::to_string(&authority).expect("typed schedule authority"),
            )
            .await
            .is_err()
            {
                return error_result("Schedule was created but its authorization evidence could not be saved; unattended additional authority requires human review.").transport_unknown();
            }
        }
        let noun = match item.kind {
            Kind::Prompt => "run",
            Kind::Text => "delivery",
        };
        let first = if item.repeat.is_some() { "first " } else { "" };
        let repeats = item
            .repeat
            .map(|repeat| format!(", repeats {}", repeat.as_str()))
            .unwrap_or_default();
        text_result(format!(
            "scheduled {}: {}, {first}{noun} {} (in {}){repeats}; delivered to {}\n{}",
            item.label(),
            item.kind.as_str(),
            format_local(item.due_at),
            format_duration(item.due_at - now),
            origin.target(),
            now_line(now),
        ))
    }
}

/// Some models set every property of a tool call and send an empty string for
/// the unused ones; a blank string counts as absent.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn build_item(
    context: &Context,
    origin: Origin,
    args: ScheduleArgs,
    now: chrono::DateTime<Utc>,
) -> Result<NewItem, String> {
    let (kind, body, max) = match (present(args.text), present(args.prompt)) {
        (Some(text), None) => (Kind::Text, text, MAX_TEXT_CHARS),
        (None, Some(prompt)) => (Kind::Prompt, prompt, MAX_PROMPT_CHARS),
        _ => return Err("give exactly one of text and prompt".into()),
    };
    let body = body.trim().to_string();
    let length = body.chars().count();
    if length == 0 || length > max {
        return Err(format!(
            "{} must be 1 to {max} characters after trimming, got {length}",
            if kind == Kind::Text { "text" } else { "prompt" }
        ));
    }
    let repeat = match present(args.repeat).as_deref() {
        None | Some("once") => None,
        Some(value) => Some(Repeat::parse(value).ok_or_else(|| {
            format!("repeat must be \"once\", \"daily\" or \"weekdays\", got {value:?}")
        })?),
    };
    let at = present(args.at);
    let (mut due_at, mut local_time) = match (args.seconds, at.as_deref()) {
        (Some(seconds), None) => {
            if !(1..=MAX_SECONDS).contains(&seconds) {
                return Err(format!("seconds must be between 1 and {MAX_SECONDS}"));
            }
            (now + TimeDelta::seconds(seconds), None)
        }
        (None, Some(at)) => resolve_at(at, now, &Local)?,
        _ => return Err("give exactly one of seconds and at".into()),
    };
    if let Some(repeat) = repeat {
        let Some(time) = local_time else {
            return Err(
                "repeat needs `at` in HH:MM form; use \"once\" or omit repeat for a one-time item"
                    .into(),
            );
        };
        // `weekdays` may skip the day `resolve_at` picked.
        due_at = next_occurrence(repeat, time, now, &Local);
    } else {
        local_time = None;
    }
    Ok(NewItem {
        origin,
        session_id: context.session_id.clone(),
        workspace: context.workspace.clone(),
        kind,
        body,
        repeat,
        local_time,
        due_at,
    })
}

struct ListTool(Context);

#[async_trait::async_trait]
impl Tool for ListTool {
    fn definition(&self) -> ToolDefinition {
        list_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if let Err(message) = decode_strict_json::<ListArgs>(arguments.get(), &[]) {
            return preflight_error(message);
        }
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED).cancelled_not_started();
        }
        let (store, origin) = match self.0.available() {
            Ok(available) => available,
            Err(result) => return result,
        };
        let now = Utc::now();
        let items = match store.list(&origin, now) {
            Ok(items) => items,
            Err(error) => return error_result(error),
        };
        let mut lines = vec![now_line(now)];
        if items.is_empty() {
            lines.push("no scheduled items".into());
        }
        lines.extend(items.iter().map(list_line));
        text_result(lines.join("\n"))
    }
}

fn list_line(item: &Item) -> String {
    let schedule = match (item.repeat, item.local_time) {
        (Some(repeat), Some(time)) => format!("{} {}", repeat.as_str(), time.format("%H:%M")),
        _ => "once".to_string(),
    };
    let status = match item.state {
        State::Waiting => format!("next {}", format_local(item.due_at)),
        State::Ready => format!("ready since {}", format_local(item.due_at)),
        State::Done => format!("done {}", format_local(item.updated_at)),
        State::Failed => format!("failed {}", format_local(item.updated_at)),
        State::Cancelled => format!("cancelled {}", format_local(item.updated_at)),
    };
    let error = if item.last_error.is_empty() {
        String::new()
    } else {
        format!(
            " [last error: {}]",
            one_line(&item.last_error, LIST_BODY_CHARS)
        )
    };
    format!(
        "{} {} {schedule} {status}: {}{error}",
        item.label(),
        item.kind.as_str(),
        one_line(&item.body, LIST_BODY_CHARS)
    )
}

struct CancelTool(Context);

#[async_trait::async_trait]
impl Tool for CancelTool {
    fn definition(&self) -> ToolDefinition {
        cancel_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: CancelArgs = match decode_strict_json(arguments.get(), &["id"]) {
            Ok(args) => args,
            Err(message) => return preflight_error(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED).cancelled_not_started();
        }
        let id = args.id.trim();
        let Some(number) = crate::schedule::parse_label(id) else {
            return preflight_error(format!("id must look like s7, got {id:?}"));
        };
        let (store, origin) = match self.0.available() {
            Ok(available) => available,
            Err(result) => return result,
        };
        match store.cancel(&origin, number, Utc::now()) {
            Ok(Some(item)) => text_result(format!(
                "cancelled {}: {}",
                item.label(),
                one_line(&item.body, LIST_BODY_CHARS)
            )),
            Ok(None) => preflight_error(format!("no active scheduled item {id} here")),
            Err(error) => error_result(error),
        }
    }
}

#[cfg(test)]
mod tests {
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
            (Origin::chat("feishu", "oc1").unwrap(), "this Feishu chat"),
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
}
