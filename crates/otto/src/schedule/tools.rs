//! The `schedule`, `schedule_list` and `schedule_cancel` tools.
//!
//! The tools read the origin of the current user turn from [`TurnOrigin`],
//! which the app layer sets at the start of each user turn. Without an origin
//! (scheduled runs, sub-agents, editor ACP clients) or without a store, every
//! call returns an error naming the cause. Items and listings are limited to
//! the current origin.

use std::sync::Arc;

use chrono::{Local, TimeDelta, Utc};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::store::{CreateError, Item, NewItem};
use super::time::{format_duration, format_local, next_occurrence, resolve_at};
use super::{Kind, Repeat, ScheduleStore, State};
use crate::tool::result::decode_strict_json;
use crate::tool::{
    ApprovalSpec, CONTEXT_CANCELED, Tool, definition, error_result, preflight_error, text_result,
};
use otto_session::origin::Origin;
use otto_session::origin::TurnOrigin;

#[cfg(test)]
pub(crate) const NAMES: [&str; 3] = ["schedule", "schedule_list", "schedule_cancel"];

const MAX_TEXT_CHARS: usize = 500;
const MAX_PROMPT_CHARS: usize = 2000;
const MAX_SECONDS: i64 = 30 * 24 * 3600;
const LIST_BODY_CHARS: usize = 80;

const NO_ORIGIN: &str = "scheduled delivery is unavailable in this conversation: it has no chat, \
    terminal or web UI origin to deliver to (scheduled runs, sub-agents and \
    editor ACP clients cannot create items)";
const NO_STORE: &str = "scheduled delivery is unavailable: ~/.otto/schedule.db could not be opened";

const SCHEDULE_DESCRIPTION: &str = "Schedule a fixed text message (`text`) or a prompt whose final reply is \
    delivered (`prompt`). Use this for a user-requested later message or \
    check; `remind` is your own timer inside this conversation. Give \
    exactly one of text/prompt and exactly one of seconds (1 to \
    2592000)/at. at is HH:MM (next occurrence) or YYYY-MM-DD HH:MM (at most \
    30 days ahead), in the host's local time zone. repeat is once \
    (default), daily or weekdays; repeats need at as HH:MM. Empty strings \
    count as absent. Returns the item id, resolved time and destination of \
    this chat, terminal or web UI, even after /new. A prompt runs in a new \
    session within its approved creation scope and frozen original user \
    evidence; generated prompt text creates no new authority. AUTO may \
    review additional ordinary operations; unresolved human choices cannot \
    run unattended.";
const LIST_DESCRIPTION: &str = "List the scheduled items of this chat or surface: active ones, then those that finished, failed or were cancelled in the last 7 days. Use the id with schedule_cancel.";
const CANCEL_DESCRIPTION: &str = "Cancel an active scheduled item by id, as schedule_list shows it (for example s7). A recurring item stops for good.";

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

pub(crate) fn tools_with_session(
    context: &Context,
    session: Arc<dyn otto_session::SessionHandle>,
) -> Vec<Box<dyn Tool + Send + Sync>> {
    vec![
        Box::new(ScheduleTool(context.clone(), Some(session))),
        Box::new(ListTool(context.clone())),
        Box::new(CancelTool(context.clone())),
    ]
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ScheduledAuthority {
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

struct ScheduleTool(Context, Option<Arc<dyn otto_session::SessionHandle>>);

pub(crate) const SCHEDULE_RETRY: (&str, otto_core::model::RetrySafety) =
    ("schedule", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for ScheduleTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        SCHEDULE_RETRY.1
    }

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
            reason: "At the due time this prompt runs the agent with its tools, including \
                file edits and sandboxed commands, without the user present. Approval \
                requests during that run are denied."
                .into(),
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

pub(crate) const SCHEDULE_LIST_RETRY: (&str, otto_core::model::RetrySafety) =
    ("schedule_list", otto_core::model::RetrySafety::ReadOnly);

#[async_trait::async_trait]
impl Tool for ListTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        SCHEDULE_LIST_RETRY.1
    }

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

pub(crate) const SCHEDULE_CANCEL_RETRY: (&str, otto_core::model::RetrySafety) = (
    "schedule_cancel",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait::async_trait]
impl Tool for CancelTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        SCHEDULE_CANCEL_RETRY.1
    }

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
mod tests;
