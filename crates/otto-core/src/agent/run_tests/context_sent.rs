//! Run-loop tests: the context report of the last request that was sent.

use super::*;
use crate::agent::context_exclusion::{ExcludedItem, ExclusionReason, NotInRequest};
use crate::agent::context_report::{ContextItem, ContextReport, ContextSection};
use crate::model::InputProvenance;
use crate::session::COMPACTION_CONTEXT_TYPE;

fn positions(sections: &[ContextSection]) -> Vec<(String, Option<u32>)> {
    let mut items: Vec<_> = sections
        .iter()
        .flat_map(|section| section.items.iter())
        .map(|item| (item.label.clone(), item.position))
        .collect();
    items.sort_by_key(|(_, position)| position.map_or(u32::MAX, |p| p));
    items
}

fn find<'a>(sections: &'a [ContextSection], kind: SectionKind) -> Option<&'a ContextSection> {
    sections.iter().find(|section| section.kind == kind)
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        ..ToolDefinition::default()
    }
}

#[test]
fn positions_follow_request_order_across_prompt_parts_tools_summary_and_messages() {
    let request = Request {
        cache_key: String::new(),
        cache_boundaries: Vec::new(),
        model: "m".into(),
        system_prompt: "AB\nrequest".into(),
        thinking: String::new(),
        messages: vec![
            Message {
                role: Role::Context,
                context_type: COMPACTION_CONTEXT_TYPE.into(),
                blocks: vec![Block::text("[Compaction summary]")],
                ..Message::default()
            },
            Message {
                role: Role::User,
                blocks: vec![Block::text("next")],
                ..Message::default()
            },
        ],
        // Built-in and MCP tools alternate; positions follow this order.
        tools: vec![tool("read"), tool("mcp__github__search"), tool("write")],
        remote_compaction: None,
        request_remote_compaction: false,
    };
    let parts = [
        ("A".to_owned(), "A".to_owned()),
        ("B".to_owned(), "B".to_owned()),
    ];
    let sections = crate::agent::context_report::sections(&request, &parts, "last recall");
    let by_position: Vec<(String, Option<u32>)> = positions(&sections);
    let expected: Vec<(String, Option<u32>)> = [
        ("A", 0),
        ("B", 1),
        ("Request context", 2),
        ("read", 3),
        ("mcp__github__search", 4),
        ("write", 5),
        ("#1 context", 6),
        ("#2 user", 7),
    ]
    .into_iter()
    .map(|(label, position)| (label.to_owned(), Some(position)))
    .chain([("Memory (last turn)".to_owned(), None)])
    .collect();
    assert_eq!(by_position, expected);
    // The sections keep their kinds: tools are split, positions are not.
    assert_eq!(find(&sections, SectionKind::Tools).unwrap().items.len(), 2);
    assert_eq!(
        find(&sections, SectionKind::McpTools).unwrap().items.len(),
        1
    );
}

#[test]
fn the_memory_item_is_the_last_message_that_carries_the_memory_text() {
    let user = |text: &str| Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    };
    let request = Request {
        cache_key: String::new(),
        cache_boundaries: Vec::new(),
        model: "m".into(),
        system_prompt: "S".into(),
        thinking: String::new(),
        // An earlier user message has the same text as the memory message.
        messages: vec![user("recall"), user("earlier"), user("recall"), user("now")],
        tools: Vec::new(),
        remote_compaction: None,
        request_remote_compaction: false,
    };
    let sections = crate::agent::context_report::sent_sections(&request, &[], "recall");
    let memory = find(&sections, SectionKind::Memory).unwrap();
    assert_eq!(memory.items.len(), 1);
    // Position 0 is the system prompt; the messages start at 1.
    assert_eq!(memory.items[0].position, Some(3));
    let messages = find(&sections, SectionKind::Messages).unwrap();
    let labels: Vec<&str> = messages
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(labels, ["#1 user", "#2 user", "#4 user"]);
}

#[test]
fn last_sent_is_none_before_the_first_request() {
    let agent = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    assert_eq!(agent.context_report().last_sent, None);
    assert_eq!(agent.context_report().not_in_request, None);
}

#[tokio::test]
async fn last_sent_is_the_last_step_with_the_memory_message_at_its_position() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        EchoExecutor {
            content: Some("the whole file".into()),
            persisted: Some("[stored elsewhere]".into()),
        },
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    agent.set_next_input_id(Some("human-1".into()));
    agent.set_next_input_provenance(Some(InputProvenance::local(crate::model::InputSource::Tui)));
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let report: ContextReport = agent.context_report();
    let sent = report.last_sent.expect("a request was sent");
    assert_eq!(sent.sent_at, 1_700_000_000_000);

    // It is the second request of the turn, as the provider received it.
    let received = agent.provider().normal_requests();
    assert_eq!(received.len(), 2);
    let request = &received[1];
    let item_count: usize = sent.sections.iter().map(|s| s.items.len()).sum();
    assert_eq!(item_count, 1 + request.tools.len() + request.messages.len());

    let memory = find(&sent.sections, SectionKind::Memory).expect("a memory section");
    assert_eq!(memory.items.len(), 1);
    let recall: &ContextItem = &memory.items[0];
    assert_eq!(recall.label, "Memory recall");
    assert!(recall.text.contains("prefers vim"));

    // The memory message is the one the request holds before the current
    // user message's input-origin label.
    let memory_index = request
        .messages
        .iter()
        .position(|message| message.text().contains("prefers vim"))
        .expect("the memory message is in the request");
    assert_eq!(
        request.messages[memory_index + 1].context_type,
        "input_origin"
    );
    assert_eq!(request.messages[memory_index + 2].text(), "hello");
    let first_message = 1 + request.tools.len();
    assert_eq!(recall.position, Some((first_message + memory_index) as u32));

    let messages = find(&sent.sections, SectionKind::Messages).expect("messages");
    assert!(
        messages
            .items
            .iter()
            .all(|item| !item.text.contains("prefers vim")),
        "the memory message is listed once"
    );
    let user = messages
        .items
        .iter()
        .find(|item| item.text == "hello")
        .unwrap();
    assert_eq!(
        user.position,
        Some((first_message + memory_index + 2) as u32)
    );

    // The overlay's full text was sent; the projected next request holds the
    // persisted text.
    let tool_text = |sections: &[ContextSection]| {
        find(sections, SectionKind::Messages)
            .unwrap()
            .items
            .iter()
            .find(|item| item.label.starts_with("#") && item.label.ends_with("tool echo"))
            .map(|item| item.text.clone())
            .expect("a tool result item")
    };
    assert_eq!(tool_text(&sent.sections), "the whole file");
    assert_eq!(tool_text(&report.sections), "[stored elsewhere]");
}

#[tokio::test]
async fn last_sent_has_no_memory_section_when_the_step_recalled_nothing() {
    // The second step has no recall, although the first one had.
    let memory = Arc::new(FakeMemory::with_records(one_record()));
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(memory.clone()),
            ..options()
        },
    );
    let cancel = CancellationToken::new();
    agent.run("first", &mut |_| {}, &cancel).await.expect("run");
    memory.set_records(Vec::new());
    agent
        .run("second", &mut |_| {}, &cancel)
        .await
        .expect("run");

    let report = agent.context_report();
    let sent = report.last_sent.expect("sent");
    assert!(find(&sent.sections, SectionKind::Memory).is_none());
}

#[tokio::test]
async fn last_sent_holds_no_value_the_redactor_removes() {
    const SECRET: &str = "sk-live-abcdef";
    let mut records = one_record();
    records[0].text = format!("prefers vim, key {SECRET}");
    let agent = Agent::with_redactor(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(records))),
            ..options()
        },
        Redactor::new(&[SECRET.to_owned()]),
    );
    agent
        .run(
            &format!("hello {SECRET}"),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let sent = agent.context_report().last_sent.expect("sent");
    let all_text: String = sent
        .sections
        .iter()
        .flat_map(|section| section.items.iter())
        .map(|item| item.text.as_str())
        .collect();
    assert!(all_text.contains("prefers vim"));
    assert!(
        !all_text.contains(SECRET),
        "the stored report holds the unguarded text"
    );
    let memory = find(&sent.sections, SectionKind::Memory).expect("memory");
    assert!(memory.items[0].text.contains("prefers vim"));
}

#[test]
fn a_report_with_last_sent_round_trips_and_an_older_report_still_decodes() {
    let section = ContextSection {
        kind: SectionKind::Messages,
        tokens: 4,
        items: vec![ContextItem {
            label: "#1 user".into(),
            tokens: 4,
            text: "hi".into(),
            position: Some(2),
        }],
    };
    let report = ContextReport {
        model: "m".into(),
        context_window: 0,
        compaction_threshold: 0,
        estimated_total: 4,
        estimate_complete: true,
        reported_input_tokens: None,
        sections: vec![section.clone()],
        last_sent: Some(crate::agent::context_report::SentRequest {
            sent_at: 5,
            sections: vec![section],
        }),
        not_in_request: None,
    };
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("not_in_request"), "{json}");
    assert_eq!(
        serde_json::from_str::<ContextReport>(&json).unwrap(),
        report
    );

    let excluded = ContextReport {
        not_in_request: Some(NotInRequest {
            items: vec![
                ExcludedItem {
                    label: "session #1 user".into(),
                    tokens: 9,
                    reason: ExclusionReason::UserMessageTruncated,
                    text: Some("old".into()),
                },
                ExcludedItem {
                    label: "session #2 assistant".into(),
                    tokens: 3,
                    reason: ExclusionReason::Summarized,
                    text: None,
                },
            ],
            carried_user_messages: 2,
        }),
        ..report
    };
    let json = serde_json::to_string(&excluded).unwrap();
    assert!(
        json.contains(r#""reason":"user_message_truncated""#),
        "{json}"
    );
    assert!(json.contains(r#""reason":"summarized""#), "{json}");
    assert_eq!(
        serde_json::from_str::<ContextReport>(&json).unwrap(),
        excluded
    );

    let older = r#"{"model":"m","context_window":0,"compaction_threshold":0,
        "estimated_total":4,"reported_input_tokens":null,
        "sections":[{"kind":"messages","tokens":4,"items":[{"label":"l","tokens":4,"text":"t"}]}]}"#;
    let decoded: ContextReport = serde_json::from_str(older).unwrap();
    assert_eq!(decoded.last_sent, None);
    assert_eq!(decoded.not_in_request, None);
    assert_eq!(decoded.sections[0].items[0].position, None);
    // New fields are omitted when absent.
    assert!(
        !serde_json::to_string(&decoded)
            .unwrap()
            .contains("position")
    );
}
