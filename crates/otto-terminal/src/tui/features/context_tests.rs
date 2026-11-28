use chrono::{Local, TimeZone};
use otto_core::agent::context_report::{
    ContextItem, ContextReport, ContextSection, SectionKind, SentRequest,
};

use super::context::Context;
use super::{Presenter, Transition};
use crate::tui::backend::Reply;
use crate::tui::interaction::{Activation, View};

const SENT_AT: i64 = 1_700_000_123_000;

fn item(label: &str, tokens: i64, text: &str, position: Option<u32>) -> ContextItem {
    ContextItem {
        label: label.into(),
        tokens,
        text: text.into(),
        position,
    }
}

fn section(kind: SectionKind, items: Vec<ContextItem>) -> ContextSection {
    ContextSection {
        kind,
        tokens: items.iter().map(|item| item.tokens).sum(),
        items,
    }
}

/// Request order of the sent request: base 0, echo 1, #1 user 2, memory 3, #3 user 4.
fn report(with_sent: bool) -> ContextReport {
    ContextReport {
        model: "m".into(),
        context_window: 0,
        compaction_threshold: 0,
        estimated_total: 500,
        estimate_complete: true,
        reported_input_tokens: None,
        sections: vec![
            section(
                SectionKind::SystemPrompt,
                vec![item("Projected base", 10, "projected", Some(0))],
            ),
            section(
                SectionKind::Memory,
                vec![item("Memory (last turn)", 3, "old recall", None)],
            ),
        ],
        last_sent: with_sent.then(|| SentRequest {
            sent_at: SENT_AT,
            sections: vec![
                section(
                    SectionKind::SystemPrompt,
                    vec![item("Base", 10, "base prompt", Some(0))],
                ),
                section(
                    SectionKind::Tools,
                    vec![item("echo", 20, "{\"name\":\"echo\"}", Some(1))],
                ),
                section(
                    SectionKind::Memory,
                    vec![item("Memory recall", 5, "User prefers VIM", Some(3))],
                ),
                section(
                    SectionKind::Messages,
                    vec![
                        item("#1 user", 100, "hello\nworld", Some(2)),
                        item("#3 user", 7, "go", Some(4)),
                    ],
                ),
            ],
        }),
    }
}

fn open(report: ContextReport) -> Context {
    let mut context = Context::new();
    context.receive(Reply::Context(report));
    context
}

fn act(context: &mut Context, id: &str) {
    context.activate(Activation::Action {
        id: id.into(),
        row: None,
        fields: Default::default(),
    });
}

fn search(context: &mut Context, query: &str) {
    context.activate(Activation::Action {
        id: "otto:search".into(),
        row: None,
        fields: [("query".to_owned(), query.to_owned())].into(),
    });
}

fn cells(view: &View) -> Vec<Vec<String>> {
    view.rows.iter().map(|row| row.cells.clone()).collect()
}

fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|cell| (*cell).to_owned()).collect()
}

fn action_enabled(view: &View, id: &str) -> bool {
    view.actions.iter().find(|a| a.id == id).unwrap().enabled
}

#[test]
fn the_basis_defaults_to_the_last_sent_request_when_there_is_one() {
    let clock = Local
        .timestamp_millis_opt(SENT_AT)
        .unwrap()
        .format("%H:%M:%S")
        .to_string();
    let view = open(report(true)).view();
    assert_eq!(
        view.notice.as_deref(),
        Some(format!("Last sent request · {clock} · ~142 tokens (estimate)").as_str())
    );
    assert_eq!(clock.len(), 8);
    assert_eq!(view.rows[0].cells[0], "System prompt");
    assert_eq!(view.rows[0].cells[1], "10");
    assert!(action_enabled(&view, "basis"));

    let view = open(report(false)).view();
    assert_eq!(
        view.notice.as_deref(),
        Some("Next request (projected; no recall for the next turn yet)")
    );
    assert!(!action_enabled(&view, "basis"));
    assert_eq!(view.rows[1].cells[0], "Memory (last turn)");
}

#[test]
fn the_basis_action_switches_between_sent_and_next_and_is_inert_without_a_sent_request() {
    let mut context = open(report(true));
    act(&mut context, "basis");
    let view = context.view();
    assert!(view.notice.unwrap().starts_with("Next request"));
    assert_eq!(view.rows.len(), 2);
    act(&mut context, "basis");
    assert!(context.view().notice.unwrap().starts_with("Last sent"));

    let mut context = open(report(false));
    act(&mut context, "basis");
    assert!(context.view().notice.unwrap().starts_with("Next request"));
}

#[test]
fn request_order_lists_items_by_position_with_the_tokens_after_each() {
    let mut context = open(report(true));
    act(&mut context, "order");
    let view = context.view();
    assert_eq!(view.columns, ["#", "Item", "Tokens", "To end"]);
    assert_eq!(
        cells(&view),
        [
            row(&["1", "system Base", "10", "132"]),
            row(&["2", "tool echo", "20", "112"]),
            row(&["3", "#1 user", "100", "12"]),
            row(&["4", "Memory recall", "5", "7"]),
            row(&["5", "#3 user", "7", "0"]),
        ]
    );
    act(&mut context, "order");
    assert_eq!(context.view().rows[0].cells[0], "System prompt");
}

#[test]
fn an_item_outside_the_request_is_listed_last_and_adds_nothing_to_the_tokens_after_it() {
    let mut context = open(report(false));
    act(&mut context, "order");
    assert_eq!(
        cells(&context.view()),
        [
            row(&["1", "system Projected base", "10", "0"]),
            row(&["—", "Memory (last turn)", "3", "—"]),
        ]
    );
}

#[test]
fn search_lists_matches_in_request_order_with_an_excerpt_and_a_count() {
    let mut context = open(report(true));
    act(&mut context, "search");
    let view = context.view();
    assert_eq!(view.search.as_deref(), Some(""));
    assert!(view.rows.is_empty());
    assert!(view.document.contains("Type to search"));

    search(&mut context, "vim");
    let view = context.view();
    assert_eq!(view.search.as_deref(), Some("vim"));
    assert!(view.description.contains("1 matches"));
    assert_eq!(cells(&view), [row(&["4", "Memory recall", "5", "7"])]);
    assert_eq!(view.rows[0].detail, "User prefers VIM");

    // Label match, ASCII case-insensitive; newlines in the excerpt are spaces.
    search(&mut context, "#1 USER");
    let view = context.view();
    assert!(view.description.contains("1 matches"));
    assert_eq!(view.rows[0].cells[1], "#1 user");
    assert_eq!(view.rows[0].detail, "hello world");
    // The text of the memory item also contains "user".
    search(&mut context, "user");
    assert!(context.view().description.contains("3 matches"));

    search(&mut context, "absent");
    let view = context.view();
    assert!(view.rows.is_empty());
    assert!(view.description.contains("0 matches"));
    assert!(view.document.contains("absent"));
}

#[test]
fn a_long_text_is_cut_around_the_first_match_on_character_boundaries() {
    let text = format!("{}needle{}", "é".repeat(60), "ü".repeat(60));
    let mut report = report(true);
    report.last_sent.as_mut().unwrap().sections[0].items[0].text = text;
    let mut context = open(report);
    act(&mut context, "search");
    search(&mut context, "NEEDLE");
    let detail = context.view().rows[0].detail.clone();
    let body = detail.trim_matches('…');
    assert_eq!(body.chars().count(), 80);
    assert!(body.contains("needle"));
    assert!(detail.starts_with('…') && detail.ends_with('…'));
    assert_eq!(body.chars().take(24).collect::<String>(), "é".repeat(24));
}

#[test]
fn backspace_on_an_empty_query_leaves_search_for_the_layout_it_started_from() {
    let mut context = open(report(true));
    act(&mut context, "order");
    act(&mut context, "search");
    search(&mut context, "v");
    // Backspace on "v" leaves an empty query and stays in search.
    search(&mut context, "");
    assert_eq!(context.view().search.as_deref(), Some(""));
    // Backspace on the empty query leaves search.
    search(&mut context, "");
    let view = context.view();
    assert_eq!(view.search, None);
    assert_eq!(view.columns.len(), 4);
    assert_eq!(view.rows.len(), 5);
}

#[test]
fn layout_and_basis_actions_do_nothing_while_searching() {
    let mut context = open(report(true));
    act(&mut context, "search");
    act(&mut context, "order");
    act(&mut context, "basis");
    act(&mut context, "search");
    let view = context.view();
    assert_eq!(view.search.as_deref(), Some(""));
    assert!(view.notice.unwrap().starts_with("Last sent"));
}

#[test]
fn enter_on_an_item_opens_its_text_in_every_mode() {
    let text_of = |context: &mut Context, id: &str| {
        let Transition::Push(document) = context.activate(Activation::Open(id.into())) else {
            panic!("a document should open");
        };
        document.view().document
    };
    let mut context = open(report(true));
    act(&mut context, "order");
    let id = context.view().rows[3].id.clone();
    assert_eq!(text_of(&mut context, &id), "User prefers VIM");
    act(&mut context, "search");
    search(&mut context, "go");
    let id = context.view().rows[0].id.clone();
    assert_eq!(text_of(&mut context, &id), "go");
    search(&mut context, "");
    search(&mut context, "");
    act(&mut context, "order");
    let section = context.view().rows[0].id.clone();
    assert!(matches!(
        context.activate(Activation::Open(section.clone())),
        Transition::Stay
    ));
    let rows = context.view().rows;
    assert_eq!(rows.len(), 5, "the first section expands to its one item");
    assert_eq!(text_of(&mut context, &rows[1].id), "base prompt");
}
