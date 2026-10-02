//! Turns a session-lease takeover into a notification the resumed agent
//! sees at the start of its next turn, and marks the child transcripts the
//! notification describes as interrupted.
//!
//! [`crate::session::Store::from_file`] repairs dangling tool calls at open
//! time and records what it did as a [`crate::session::Takeover`]; the
//! parent's own sub-agent children are not part of that record, since they
//! are separate transcripts, so this module re-derives their state with
//! [`crate::subagent::interrupted::scan`]. [`notify`] is called once, from
//! `cli::wiring::Builder::build_subagents`, after sub-agent wiring is up: a
//! session that never takes over a lease, or whose caller disables
//! sub-agents entirely (`build_subagents` returns before an inbox exists),
//! never reaches it, so no notification is pushed in either case.

use std::path::Path;

use otto_core::agent::inbox::{Inbox, Notification, NotificationKind};
use otto_core::model::{Message, Role};

use crate::failover::lease;
use crate::session::{Takeover, UnansweredCall};
use crate::subagent::interrupted::{self, ChildRecord};

/// Truncation limits for the pieces of the recovery text that come from
/// unbounded user or model input, so one runaway prompt or argument list
/// cannot make the notification itself unbounded.
const CALL_ARGUMENTS_PREVIEW_BYTES: usize = 2048;
const PROMPT_PREVIEW_BYTES: usize = 500;

/// Reports the takeover `session` recorded (see
/// [`crate::session::Store::take_takeover`]) to `inbox`, and marks every
/// interrupted child transcript under `children_dir` as interrupted. A
/// no-op when there was no takeover, or when the takeover left nothing to
/// report: no dangling parent call, no interrupted child, and the parent's
/// own transcript already ends on a finished assistant turn.
///
/// The notification push always happens first, so the model sees the
/// recovery notice (including a `resume` pointer for the child) even when a
/// following `mark_interrupted` call fails. Each such failure is returned as
/// one warning string naming the task id, the transcript path and the
/// error, since a `resume` call against a transcript that is not marked
/// interrupted is rejected, and the caller is the only place that can report
/// why.
pub fn notify(
    inbox: &Inbox,
    takeover: Option<Takeover>,
    children_dir: &Path,
    max_output_bytes: usize,
    parent_messages: &[Message],
) -> Vec<String> {
    let Some(takeover) = takeover else {
        return Vec::new();
    };
    let interrupted_children: Vec<ChildRecord> = interrupted::scan(children_dir)
        .into_iter()
        .filter(|child| child.final_status.is_none())
        .collect();

    if takeover.repaired.is_empty()
        && takeover.replayable.is_empty()
        && interrupted_children.is_empty()
        && ends_with_a_finished_assistant_turn(parent_messages)
    {
        return Vec::new();
    }

    let text = render(&takeover, &interrupted_children, max_output_bytes);
    inbox.push(Notification {
        task_id: String::new(),
        kind: Some(NotificationKind::Message),
        text,
        usage: None,
    });

    let mut warnings = Vec::new();
    for child in &interrupted_children {
        if let Err(error) = interrupted::mark_interrupted(&child.path) {
            warnings.push(format!(
                "recovery: task {} ({}): failed to mark interrupted: {error}",
                child.task_id,
                child.path.display()
            ));
        }
    }
    warnings
}

/// Reports a SIGTERM migration's cancelled sub-agent tasks to `inbox`: the
/// tasks named in `task_ids` were cancelled by
/// [`crate::subagent::tasks::Tasks::begin_migration`] and already marked
/// interrupted by `subagent::runner::Runner::finish`'s own migration branch,
/// through their own already-open transcript `Store`, so unlike [`notify`]
/// this never calls [`interrupted::mark_interrupted`] itself.
///
/// A no-op when nothing was migrated and the parent's own transcript already
/// ends on a finished assistant turn. An id in `task_ids` whose transcript is
/// not recorded as interrupted (the task finished on its own right before
/// the migration cancelled it, or its transcript write failed) is reported
/// as one warning instead of being included in the notification.
pub fn notify_moved(
    inbox: &Inbox,
    children_dir: &Path,
    task_ids: &[String],
    max_output_bytes: usize,
    parent_messages: &[Message],
) -> Vec<String> {
    let scanned = interrupted::scan(children_dir);
    let mut moved = Vec::new();
    let mut warnings = Vec::new();
    for id in task_ids {
        match scanned.iter().find(|child| {
            &child.task_id == id
                && child.final_status.as_deref() == Some(interrupted::INTERRUPTED_STATUS)
        }) {
            Some(child) => moved.push(child.clone()),
            None => warnings.push(format!("migration: task {id}: not recorded as interrupted")),
        }
    }

    if moved.is_empty() && ends_with_a_finished_assistant_turn(parent_messages) {
        return warnings;
    }

    let header = format!(
        "This session was moved: host {} pid {} received SIGTERM and cancelled its running tool calls and sub-agent tasks.\n",
        lease::local_hostname(),
        std::process::id(),
    );
    let text = render_body(&header, &[], &[], &moved, max_output_bytes);
    inbox.push(Notification {
        task_id: String::new(),
        kind: Some(NotificationKind::Message),
        text,
        usage: None,
    });
    warnings
}

fn ends_with_a_finished_assistant_turn(messages: &[Message]) -> bool {
    matches!(
        messages.last(),
        Some(message) if message.role == Role::Assistant && !message.has_tool_call()
    )
}

fn render(takeover: &Takeover, interrupted: &[ChildRecord], max_output_bytes: usize) -> String {
    let header = format!(
        "This session was taken over from another host: the previous holder was fenced at epoch {}, host {}, pid {}.\n",
        takeover.holder.epoch, takeover.holder.host, takeover.holder.pid
    );
    render_body(
        &header,
        &takeover.repaired,
        &takeover.replayable,
        interrupted,
        max_output_bytes,
    )
}

/// The body shared by [`render`] and [`notify_moved`]: a header line
/// naming what happened, the repaired-call section, one section per
/// interrupted child, and the resume footer.
fn render_body(
    header: &str,
    repaired: &[UnansweredCall],
    replayed: &[UnansweredCall],
    interrupted: &[ChildRecord],
    max_output_bytes: usize,
) -> String {
    let mut text = header.to_string();

    if !replayed.is_empty() {
        text.push_str(
            "\nRead-only calls left unanswered when the session was taken over were run \
             again; their results are in the transcript above and reflect the workspace now:\n",
        );
        for call in replayed {
            text.push_str(&format!(
                "- {} {}\n",
                call.name,
                truncate_bytes(&call.arguments, CALL_ARGUMENTS_PREVIEW_BYTES)
            ));
        }
    }

    if !repaired.is_empty() {
        text.push_str("\nCalls left unanswered in this session when it was taken over:\n");
        for call in repaired {
            text.push_str(&render_call(call));
        }
    }

    for child in interrupted {
        text.push_str(&format!(
            "\nInterrupted task {} \"{}\" (agent: {}): {}\n",
            child.task_id,
            child.name,
            if child.agent.is_empty() {
                "default"
            } else {
                &child.agent
            },
            child.description,
        ));
        text.push_str(&format!(
            "prompt: {}\n",
            truncate_bytes(&child.prompt, PROMPT_PREVIEW_BYTES)
        ));
        text.push_str(&format!("started: {}\n", child.started));
        if !child.unanswered.is_empty() {
            text.push_str("calls left unanswered:\n");
            for call in &child.unanswered {
                text.push_str(&render_call(call));
            }
        }
        text.push_str(&format!(
            "last assistant text: {}\n",
            truncate_bytes(&child.last_assistant_text, max_output_bytes)
        ));
    }

    text.push_str(
        "\nTo continue an interrupted task, call agent with resume set to its task id and a \
         prompt. A task not continued this way stays interrupted.\n",
    );
    text
}

fn render_call(call: &UnansweredCall) -> String {
    let status = if call.may_have_run {
        "effects unknown; may have run; do not retry automatically"
    } else {
        "not executed"
    };
    format!(
        "- {} {} ({status})\n",
        call.name,
        truncate_bytes(&call.arguments, CALL_ARGUMENTS_PREVIEW_BYTES)
    )
}

/// `text` truncated to at most `max_bytes` bytes, cut back to the nearest
/// character boundary so the result is always valid UTF-8.
fn truncate_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failover::lease;
    use otto_core::model::{Block, FinishReason};

    fn takeover(repaired: Vec<UnansweredCall>) -> Takeover {
        Takeover {
            holder: lease::Holder {
                epoch: 3,
                host: "old-host".to_string(),
                pid: 4242,
            },
            repaired,
            replayable: Vec::new(),
        }
    }

    fn call(name: &str, arguments: &str, may_have_run: bool) -> UnansweredCall {
        UnansweredCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
            may_have_run,
        }
    }

    fn interrupted_child(task_id: &str) -> ChildRecord {
        ChildRecord {
            path: std::path::PathBuf::from(format!("/tmp/{task_id}.jsonl")),
            task_id: task_id.to_string(),
            name: "worker".to_string(),
            agent: "reviewer".to_string(),
            description: "review the change".to_string(),
            prompt: "x".repeat(600),
            model: "test-model".to_string(),
            context: "fresh".to_string(),
            definition: None,
            started: true,
            unanswered: vec![call("bash", "{\"cmd\":\"y\"}", true)],
            last_assistant_text: "z".repeat(20),
            final_status: None,
        }
    }

    fn assistant(text: &str, tool_call: bool) -> Message {
        Message {
            role: Role::Assistant,
            blocks: if tool_call {
                vec![Block {
                    block_type: otto_core::model::BlockType::ToolCall,
                    tool_call_id: "call-1".into(),
                    tool_name: "bash".into(),
                    arguments: Some(
                        serde_json::value::RawValue::from_string("{}".into()).expect("valid"),
                    ),
                    ..Block::default()
                }]
            } else {
                vec![Block::text(text)]
            },
            finish_reason: Some(FinishReason::Stop),
            ..Message::default()
        }
    }

    #[test]
    fn the_rendered_text_names_every_reported_item_and_truncates_at_the_documented_limits() {
        let takeover = takeover(vec![call("write", &"a".repeat(3000), false)]);
        let child = interrupted_child("t1");
        let text = render(&takeover, std::slice::from_ref(&child), 10);

        assert!(text.contains("epoch 3"), "{text}");
        assert!(text.contains("old-host"), "{text}");
        assert!(text.contains("4242"), "{text}");

        assert!(text.contains("write"), "{text}");
        assert!(text.contains("not executed"), "{text}");
        let arguments_line = text.lines().find(|line| line.contains("write")).unwrap();
        assert_eq!(
            arguments_line.matches('a').count(),
            CALL_ARGUMENTS_PREVIEW_BYTES,
            "the repaired call's arguments must be truncated to 2 KiB"
        );

        assert!(text.contains("t1"), "{text}");
        assert!(text.contains("worker"), "{text}");
        assert!(text.contains("reviewer"), "{text}");
        assert!(text.contains("review the change"), "{text}");
        assert!(text.contains(&"x".repeat(PROMPT_PREVIEW_BYTES)), "{text}");
        assert!(
            !text.contains(&"x".repeat(PROMPT_PREVIEW_BYTES + 1)),
            "the prompt must be truncated to 500 bytes: {text}"
        );
        assert!(text.contains("started: true"), "{text}");
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("may have run"), "{text}");
        assert!(text.contains("do not retry automatically"), "{text}");
        assert!(text.contains(&"z".repeat(10)), "{text}");
        assert!(
            !text.contains(&"z".repeat(11)),
            "the last assistant text must be truncated to max_output_bytes: {text}"
        );

        assert!(text.contains("resume"), "{text}");
        assert!(text.contains("stays interrupted"), "{text}");
    }

    #[test]
    fn notify_does_nothing_without_a_takeover() {
        let inbox = Inbox::new(None);
        notify(&inbox, None, Path::new("/does/not/exist"), 1000, &[]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn notify_pushes_nothing_when_the_parent_ended_on_a_finished_assistant_turn_and_nothing_else_is_wrong()
     {
        let inbox = Inbox::new(None);
        let dir = tempfile::tempdir().expect("dir");
        notify(
            &inbox,
            Some(takeover(Vec::new())),
            &dir.path().join("children"),
            1000,
            &[assistant("done", false)],
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn notify_pushes_when_the_parent_ended_mid_tool_call_even_with_no_children() {
        let inbox = Inbox::new(None);
        let dir = tempfile::tempdir().expect("dir");
        notify(
            &inbox,
            Some(takeover(Vec::new())),
            &dir.path().join("children"),
            1000,
            &[assistant("", true)],
        );
        assert_eq!(inbox.len(), 1);
    }

    #[test]
    fn notify_lists_replayed_calls_apart_from_unanswered_ones() {
        let inbox = Inbox::new(None);
        let dir = tempfile::tempdir().expect("dir");
        let mut record = takeover(Vec::new());
        record.replayable = vec![call("read", "{\"path\":\"a.txt\"}", true)];
        notify(
            &inbox,
            Some(record),
            &dir.path().join("children"),
            1000,
            &[assistant("", true)],
        );
        let queued = inbox.queued();
        assert_eq!(queued.len(), 1);
        let text = &queued[0].notification.text;
        assert!(text.contains("were run again"), "{text}");
        assert!(text.contains("- read {\"path\":\"a.txt\"}"), "{text}");
        assert!(
            !text.contains("Calls left unanswered"),
            "a replayed call is not reported as unanswered: {text}"
        );
        assert!(!text.contains("do not retry automatically"), "{text}");
    }

    /// Writes a child transcript at `<parent without .jsonl>/<task_id>-child.jsonl`
    /// with an `otto.task_spec` entry, and, when `status` is `Some`, a
    /// trailing `otto.task_result` entry carrying it, matching the shape
    /// [`interrupted::scan`] reads.
    fn child_with_result(parent: &Path, task_id: &str, prompt: &str, status: Option<&str>) {
        let workspace = parent.parent().expect("dir");
        let child = crate::session::Store::create_child_lazy(
            parent,
            &format!("{task_id}-child"),
            otto_core::session::Header {
                id: "child".into(),
                workspace: workspace.to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("create child");
        child
            .append_custom_entry(
                crate::subagent::runner::TASK_SPEC_CUSTOM_TYPE,
                &serde_json::json!({
                    "id": task_id,
                    "name": "worker",
                    "description": "review the change",
                    "model": "test-model",
                    "context": "fresh",
                    "prompt": prompt,
                    "definition": null,
                })
                .to_string(),
            )
            .expect("append task_spec");
        if let Some(status) = status {
            child
                .append_custom_entry(
                    crate::subagent::runner::TASK_RESULT_CUSTOM_TYPE,
                    &serde_json::json!({"status": status, "error": ""}).to_string(),
                )
                .expect("append task_result");
        }
        child.close().expect("close child");
    }

    #[test]
    fn notify_moved_names_the_host_pid_and_each_kept_task() {
        let root = tempfile::tempdir().expect("root");
        let parent_path = root.path().join("parent.jsonl");
        child_with_result(
            &parent_path,
            "t1",
            "do the thing",
            Some(interrupted::INTERRUPTED_STATUS),
        );

        let inbox = Inbox::new(None);
        let children_dir = parent_path.with_extension("");
        let warnings = notify_moved(&inbox, &children_dir, &["t1".to_string()], 1000, &[]);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(inbox.len(), 1);
        let text = inbox.snapshot()[0].text.clone();
        assert!(text.contains("moved"), "{text}");
        assert!(text.contains(&lease::local_hostname()), "{text}");
        assert!(text.contains(&std::process::id().to_string()), "{text}");
        assert!(text.contains("t1"), "{text}");
        assert!(text.contains("worker"), "{text}");
        assert!(text.contains("do the thing"), "{text}");
        assert!(text.contains("stays interrupted"), "{text}");
    }

    #[test]
    fn notify_moved_reports_a_task_id_not_recorded_as_interrupted_as_one_warning() {
        let root = tempfile::tempdir().expect("root");
        let parent_path = root.path().join("parent.jsonl");

        let inbox = Inbox::new(None);
        let children_dir = parent_path.with_extension("");
        let warnings = notify_moved(
            &inbox,
            &children_dir,
            &["missing".to_string()],
            1000,
            &[assistant("done", false)],
        );

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("missing"), "{warnings:?}");
        assert!(
            inbox.is_empty(),
            "no kept task plus a finished parent turn must push nothing"
        );
    }

    #[test]
    fn notify_moved_pushes_nothing_with_no_kept_task_and_a_finished_parent_turn() {
        let inbox = Inbox::new(None);
        let dir = tempfile::tempdir().expect("dir");
        let warnings = notify_moved(
            &inbox,
            &dir.path().join("children"),
            &[],
            1000,
            &[assistant("done", false)],
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(inbox.is_empty());
    }

    #[test]
    fn notify_moved_pushes_one_notification_with_no_kept_task_and_a_parent_turn_mid_tool_call() {
        let inbox = Inbox::new(None);
        let dir = tempfile::tempdir().expect("dir");
        let warnings = notify_moved(
            &inbox,
            &dir.path().join("children"),
            &[],
            1000,
            &[assistant("", true)],
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(inbox.len(), 1);
    }

    /// W2: `mark_interrupted`'s failure must not be discarded. The
    /// notification is still pushed (push happens before the mark), and the
    /// failure comes back as one warning naming the task id, so
    /// `build_subagents` can report why a later `resume` call is rejected.
    #[test]
    fn a_child_whose_mark_interrupted_fails_still_gets_notified_and_the_failure_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("root");
        let parent_path = root.path().join("parent.jsonl");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");

        let child = crate::session::Store::create_child_lazy(
            &parent_path,
            "t1-child",
            otto_core::session::Header {
                id: "child".into(),
                workspace: workspace.to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("create child");
        child
            .append_custom_entry(
                crate::subagent::runner::TASK_SPEC_CUSTOM_TYPE,
                &serde_json::json!({
                    "id": "t1",
                    "name": "worker",
                    "description": "",
                    "model": "test-model",
                    "context": "fresh",
                    "prompt": "do it",
                    "definition": null,
                })
                .to_string(),
            )
            .expect("append task_spec");
        child.close().expect("close child");

        let child_path = crate::session::Store::child_path(&parent_path, "t1-child");
        let original_mode = std::fs::metadata(&child_path)
            .expect("stat child")
            .permissions()
            .mode();
        std::fs::set_permissions(&child_path, std::fs::Permissions::from_mode(0o444))
            .expect("make child read-only");

        let inbox = Inbox::new(None);
        let children_dir = parent_path.with_extension("");
        let warnings = notify(&inbox, Some(takeover(Vec::new())), &children_dir, 1000, &[]);

        // Restore write access before the TempDir is dropped, regardless of
        // whether the assertions below panic.
        let _ =
            std::fs::set_permissions(&child_path, std::fs::Permissions::from_mode(original_mode));

        assert_eq!(inbox.len(), 1, "the notification must still be pushed");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("t1"), "{warnings:?}");
    }
}
