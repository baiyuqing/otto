//! Each test builds a session log with the real entry encoder, resolves the
//! request messages with `build_context`, the Bash history projection and the
//! input provenance projection (the steps of `build_normal_provider_request`
//! that change message text), and compares both with `not_in_request`.

use chrono::DateTime;

use super::*;
use crate::agent::bash_history;
use crate::agent::context_report::sections;
use crate::agent::summary_details::{append_compaction_blocks, carry_user_messages};
use crate::model::{
    Block, BlockType, FinishReason, InputProvenance, InputSource, Message, Role,
    project_input_provenance,
};
use crate::provider::Request;
use crate::session::context::model_message_to_pi_entry;
use crate::session::pi::PiCompaction;
use crate::session::{CompactionDetails, Header, build_context, decode_pi_entry};

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        finish_reason: Some(FinishReason::Stop),
        ..Message::default()
    }
}

fn call(id: &str, name: &str, arguments: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments: Some(
                serde_json::value::RawValue::from_string(arguments.into()).expect("arguments"),
            ),
            ..Block::default()
        }],
        finish_reason: Some(FinishReason::ToolCalls),
        ..Message::default()
    }
}

fn result(id: &str, name: &str, text: &str) -> Message {
    Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            tool_call_id: id.into(),
            tool_name: name.into(),
            text: text.into(),
            ..Block::default()
        }],
        ..Message::default()
    }
}

struct Log {
    entries: Vec<PiEntry>,
    header: Header,
}

impl Log {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            header: Header {
                provider: "provider".into(),
                model: "model".into(),
                ..Header::default()
            },
        }
    }

    fn next_id(&self) -> String {
        format!("{:08x}", self.entries.len() + 1)
    }

    fn parent(&self) -> Option<String> {
        self.entries.last().map(|entry| entry.id.clone())
    }

    fn timestamped(&self, mut message: Message) -> Message {
        let seconds = 1_700_000_000 + self.entries.len() as i64;
        message.created_at = DateTime::from_timestamp(seconds, 0).expect("in range");
        message
    }

    /// Appends `message` as the next entry and returns the entry id.
    fn push(&mut self, message: Message) -> String {
        let id = self.next_id();
        let message = self.timestamped(message);
        let (entry, _) =
            model_message_to_pi_entry(&message, &id, self.parent().as_deref(), &self.header)
                .expect("encode message");
        self.entries.push(entry);
        id
    }

    fn push_all(&mut self, messages: impl IntoIterator<Item = Message>) -> Vec<String> {
        messages
            .into_iter()
            .map(|message| self.push(message))
            .collect()
    }

    /// Appends a checkpoint. `summarized` are the messages it replaces; the
    /// user messages among them go into the summary's `<user-messages>` block
    /// the way `Agent::compact` writes them. With `first_kept` the checkpoint
    /// anchors on that entry; with `tail` it carries the messages itself.
    fn compact(&mut self, summarized: &[&Message], first_kept: Option<&str>, tail: &[Message]) {
        let lines = carry_user_messages(&[], summarized.iter().copied());
        let summary = append_compaction_blocks(
            "summary text",
            &lines,
            &[],
            &[],
            &CompactionDetails::default(),
        )
        .expect("summary");
        let id = self.next_id();
        let timestamp = self
            .timestamped(Message::default())
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut entry = PiEntry::new("compaction", &id, self.parent(), &timestamp);
        let retained_tail = (!tail.is_empty()).then(|| {
            tail.iter()
                .map(|message| {
                    let message = self.timestamped(message.clone());
                    let (entry, _) =
                        model_message_to_pi_entry(&message, "00000fff", None, &self.header)
                            .expect("encode tail message");
                    *entry.message.expect("message payload")
                })
                .collect()
        });
        entry.compaction = Some(Box::new(PiCompaction {
            summary,
            first_kept_entry_id: first_kept.map(str::to_owned),
            tokens_before: 100,
            retained_tail,
            ..PiCompaction::default()
        }));
        self.entries.push(entry);
    }

    /// Appends a `context_edit` entry through the record decoder.
    fn edit(&mut self, target: &str, replacement: Option<&str>) {
        let replacement = replacement.map_or_else(
            || "null".to_owned(),
            |text| format!(r#"{{"content":{}}}"#, serde_json::json!(text)),
        );
        let parent = self
            .parent()
            .map_or("null".into(), |id| format!("\"{id}\""));
        let record = format!(
            r#"{{"type":"context_edit","id":"{}","parentId":{parent},"timestamp":"2026-08-27T12:00:00Z","targetId":"{target}","replacement":{replacement}}}"#,
            self.next_id()
        );
        self.entries
            .push(decode_pi_entry(record.as_bytes()).expect("decode context_edit"));
    }

    /// The message sections of the request the agent would build for this log.
    fn request_sections(&self) -> Vec<ContextSection> {
        let leaf = self.entries.last().expect("entries").id.clone();
        let (context, _) = build_context(&self.entries, &leaf).expect("build context");
        let mut messages = context.model_messages;
        bash_history::summarize(&mut messages);
        let request = Request {
            messages: project_input_provenance(messages),
            ..Request::default()
        };
        sections(&request, &[], "")
    }

    fn not_in_request(&self) -> NotInRequest {
        not_in_request_for_entries(&self.entries, &self.request_sections())
            .expect("compare with the request")
    }
}

fn labelled(excluded: &NotInRequest) -> Vec<(&str, ExclusionReason)> {
    excluded
        .items
        .iter()
        .map(|item| (item.label.as_str(), item.reason))
        .collect()
}

#[test]
fn a_session_without_a_checkpoint_or_edit_has_no_excluded_message() {
    let mut log = Log::new();
    let tui = Some(InputProvenance::local(InputSource::Tui));
    log.push(Message {
        input_provenance: tui,
        ..user("list the files")
    });
    log.push(call("call-1", "ls", r#"{"path":"."}"#));
    log.push(result("call-1", "ls", "a.txt\nb.txt"));
    log.push(assistant("two files"));
    log.push(Message {
        input_provenance: tui,
        ..user("thanks")
    });
    log.push(assistant("welcome"));

    let request = log.request_sections();
    let request_texts: Vec<&str> = request
        .iter()
        .flat_map(|section| &section.items)
        .map(|item| item.text.as_str())
        .collect();
    assert!(
        request_texts
            .iter()
            .any(|text| text.starts_with("Runtime origin for the following user input")),
        "the request carries the provenance label, which is not a session message"
    );
    assert_eq!(
        log.not_in_request(),
        NotInRequest {
            items: Vec::new(),
            carried_user_messages: 0
        }
    );
}

#[test]
fn an_anchor_checkpoint_lists_the_summarized_messages_and_counts_carried_user_messages() {
    let mut log = Log::new();
    let first = user("first question");
    let older = [
        first.clone(),
        call("call-1", "bash", r#"{"command":"ls"}"#),
        result("call-1", "bash", "a.txt"),
        assistant("the answer"),
    ];
    log.push_all(older.clone());
    let kept = log.push(user("second question"));
    log.push(assistant("second answer"));
    log.compact(&older.iter().collect::<Vec<_>>(), Some(&kept), &[]);
    log.push(user("third question"));

    let excluded = log.not_in_request();
    assert_eq!(
        labelled(&excluded),
        [
            ("session #2 assistant bash", ExclusionReason::Summarized),
            ("session #3 tool bash", ExclusionReason::Summarized),
            ("session #4 assistant", ExclusionReason::Summarized),
        ]
    );
    assert_eq!(
        excluded.carried_user_messages, 1,
        "first question is carried"
    );
    assert_eq!(
        excluded.items[2].text.as_deref(),
        Some("the answer"),
        "the text is the rendered message"
    );
    assert!(excluded.items.iter().all(|item| item.tokens > 0));
}

#[test]
fn a_retained_tail_checkpoint_does_not_list_the_tail_messages() {
    let mut log = Log::new();
    let older = [
        user("old question"),
        assistant("old answer"),
        user("tail question"),
        assistant("tail answer"),
    ];
    log.push_all(older.clone());
    log.compact(&older[..2].iter().collect::<Vec<_>>(), None, &older[2..]);
    log.push(user("after the checkpoint"));

    let excluded = log.not_in_request();
    assert_eq!(
        labelled(&excluded),
        [("session #2 assistant", ExclusionReason::Summarized)]
    );
    assert_eq!(excluded.carried_user_messages, 1);
}

#[test]
fn a_user_message_is_listed_as_cut_or_dropped_when_the_summary_does_not_carry_it_whole() {
    let mut log = Log::new();
    // 12 messages of 1,900 characters and a 13th longer than 2,000: the
    // 16 KiB block holds the cut 13th and the newest 7 of the others.
    let mut older: Vec<Message> = (0..12)
        .map(|index| user(&format!("{index:02}{}", "x".repeat(1_898))))
        .collect();
    let long = format!(
        "{}{}",
        "y".repeat(USER_MESSAGE_MAXIMUM_RUNES),
        "tail beyond the cut"
    );
    older.push(user(&long));
    log.push_all(older.iter().cloned().flat_map(|message| {
        let reply = assistant("noted");
        [message, reply]
    }));
    let kept = log.push(user("kept"));
    log.compact(&older.iter().collect::<Vec<_>>(), Some(&kept), &[]);

    let excluded = log.not_in_request();
    let reasons = |reason: ExclusionReason| {
        excluded
            .items
            .iter()
            .filter(|item| item.reason == reason)
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        reasons(ExclusionReason::UserMessageTruncated),
        ["session #25 user"]
    );
    assert_eq!(
        reasons(ExclusionReason::UserMessageDropped),
        [
            "session #1 user",
            "session #3 user",
            "session #5 user",
            "session #7 user",
            "session #9 user"
        ]
    );
    assert_eq!(reasons(ExclusionReason::Projected), Vec::<&str>::new());
    assert_eq!(excluded.carried_user_messages, 7);
    assert_eq!(
        reasons(ExclusionReason::Summarized).len(),
        13,
        "the 13 assistant replies"
    );
}

#[test]
fn a_context_edit_lists_the_original_as_projected() {
    let mut log = Log::new();
    let edited = log.push(user("original secret-free text"));
    let removed = log.push(user("to be removed"));
    log.push(assistant("reply"));
    log.edit(&edited, Some("replacement text"));
    log.edit(&removed, None);

    let excluded = log.not_in_request();
    assert_eq!(
        labelled(&excluded),
        [
            ("session #1 user", ExclusionReason::Projected),
            ("session #2 user", ExclusionReason::Projected),
        ]
    );
    assert_eq!(
        excluded.items[0].text.as_deref(),
        Some("original secret-free text")
    );
}

#[test]
fn historical_bash_output_is_listed_as_projected() {
    let mut log = Log::new();
    log.push(user("run it"));
    log.push(call("call-1", "bash", r#"{"command":"make"}"#));
    log.push(result(
        "call-1",
        "bash",
        "stdout:\nbuilt\nstderr:\n\nexit_code: 0",
    ));
    log.push(assistant("built"));

    let excluded = log.not_in_request();
    assert_eq!(
        labelled(&excluded),
        [("session #3 tool bash", ExclusionReason::Projected)]
    );
}

#[test]
fn an_identical_text_before_the_boundary_does_not_take_the_match_of_a_kept_message() {
    let mut log = Log::new();
    let older = [user("question"), assistant("ok")];
    log.push_all(older.clone());
    let kept = log.push(user("next"));
    log.push(assistant("ok"));
    log.compact(&older.iter().collect::<Vec<_>>(), Some(&kept), &[]);

    let excluded = log.not_in_request();
    assert_eq!(
        labelled(&excluded),
        [("session #2 assistant", ExclusionReason::Summarized)],
        "the earlier `ok` is listed, the kept one is in the request"
    );
}

#[test]
fn equal_messages_are_matched_one_to_one() {
    let mut log = Log::new();
    log.push(user("same"));
    log.push(assistant("reply"));
    log.push(user("same"));
    // Remove the first one from the request.
    let first = log.entries[0].id.clone();
    log.edit(&first, None);

    let excluded = log.not_in_request();
    assert_eq!(excluded.items.len(), 1, "{:?}", labelled(&excluded));
    assert_eq!(excluded.items[0].reason, ExclusionReason::Projected);
}

#[test]
fn text_over_the_cap_is_dropped_oldest_first_and_the_tokens_stay() {
    let mut log = Log::new();
    let size = NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES / 3 + 1;
    let older: Vec<Message> = (0..3)
        .flat_map(|index| {
            [
                user(&format!("question {index}")),
                assistant(&format!("{index}{}", "z".repeat(size))),
            ]
        })
        .collect();
    log.push_all(older.clone());
    let kept = log.push(user("kept"));
    log.compact(&older.iter().collect::<Vec<_>>(), Some(&kept), &[]);

    let excluded = log.not_in_request();
    assert_eq!(excluded.items.len(), 3);
    let kept_text: Vec<bool> = excluded
        .items
        .iter()
        .map(|item| item.text.is_some())
        .collect();
    assert_eq!(kept_text, [false, true, true]);
    assert!(
        excluded.items[0].tokens > 500_000,
        "{}",
        excluded.items[0].tokens
    );
    let total: usize = excluded
        .items
        .iter()
        .filter_map(|item| item.text.as_deref())
        .map(str::len)
        .sum();
    assert!(total <= NOT_IN_REQUEST_TEXT_MAXIMUM_BYTES);
}

#[test]
fn only_the_active_branch_is_compared() {
    let mut log = Log::new();
    log.push(user("root"));
    let side = log.push(assistant("abandoned branch"));
    log.push(assistant("active branch"));
    // Re-parent the last entry to the root, leaving the first assistant on a
    // side branch.
    let root = log.entries[0].id.clone();
    log.entries[2].parent_id = Some(root);
    assert_ne!(log.entries[1].id, log.entries[2].id);
    assert!(log.entries.iter().any(|entry| entry.id == side));

    assert!(log.not_in_request().items.is_empty());
}
