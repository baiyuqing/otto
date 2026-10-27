//! Node inputs, zoom and date.

use crate::view_tree::*;

fn message(id: u64, kind: LogKind, text: &str) -> LogMessage {
    LogMessage {
        id,
        kind,
        text: text.into(),
        entry_id: format!("e{id}"),
        timestamp: format!("2026-10-27T00:00:0{id}Z"),
    }
}

fn small_log() -> Vec<LogMessage> {
    vec![
        message(0, LogKind::User, "fix the bug"),
        message(1, LogKind::Assistant, "looking"),
        message(2, LogKind::ToolCall, "read {}"),
        message(3, LogKind::ToolResult, "ok"),
    ]
}

fn built_tree(log: &[LogMessage]) -> SummaryTree {
    let mut tree = SummaryTree::new();
    for address in SummaryTree::addresses_for_log(log.len() as u64) {
        let NodeInput::Ready(text) = tree.node_input(log, address).unwrap() else {
            panic!("{address} needs a model")
        };
        tree.insert(address, text).unwrap();
    }
    tree
}

#[test]
fn addresses_cover_every_aligned_range_inside_the_log() {
    let ids: Vec<String> = SummaryTree::addresses_for_log(5)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        ids,
        ["0+1", "1+1", "2+1", "3+1", "4+1", "0+2", "2+2", "0+4"]
    );
}

#[test]
fn node_address_relations() {
    let node = NodeAddress::new(6, 2).unwrap();
    assert_eq!((node.level(), node.last_id(), node.end_id()), (1, 7, 8));
    assert_eq!(node.parent(), NodeAddress { id: 4, n: 4 });
    assert_eq!(node.sibling(), NodeAddress { id: 4, n: 2 });
    assert_eq!(
        node.children(),
        Some((NodeAddress { id: 6, n: 1 }, NodeAddress { id: 7, n: 1 }))
    );
    assert!(node.is_sibling_of(&NodeAddress { id: 4, n: 2 }));
    assert!(!node.is_sibling_of(&NodeAddress { id: 8, n: 2 }));
    assert_eq!(node.to_string(), "6+2");
    assert!(
        NodeAddress::new(3, 2).is_err()
            && NodeAddress::new(0, 3).is_err()
            && NodeAddress::new(0, 0).is_err()
    );
}

#[test]
fn inputs_that_fit_in_a_node_skip_the_model() {
    let mut log = small_log();
    log.push(message(4, LogKind::Assistant, &"w".repeat(600)));
    log.push(message(5, LogKind::User, "short"));
    let mut tree = built_tree(&log[..4]);
    assert_eq!(tree.get(NodeAddress::leaf(0)), Some("user: fix the bug"));
    assert_eq!(
        tree.get(NodeAddress { id: 0, n: 2 }),
        Some("user: fix the bug\nassistant: looking")
    );
    // A leaf over 512 bytes needs a call; a short one does not.
    assert!(
        matches!(tree.node_input(&log, NodeAddress::leaf(4)).unwrap(), NodeInput::Leaf(task) if task.kind == LogKind::Assistant)
    );
    assert_eq!(
        tree.node_input(&log, NodeAddress::leaf(5)).unwrap(),
        NodeInput::Ready("user: short".into())
    );
    // Two child texts that together exceed 512 bytes need a merge call.
    tree.insert(NodeAddress::leaf(4), "a".repeat(300)).unwrap();
    tree.insert(NodeAddress::leaf(5), "b".repeat(300)).unwrap();
    let NodeInput::Merge(task) = tree.node_input(&log, NodeAddress { id: 4, n: 2 }).unwrap() else {
        panic!("merge expected")
    };
    assert_eq!((task.first.len(), task.second.len()), (300, 300));
    // Children not built yet, a duplicate insert and an out-of-log node are errors.
    assert!(tree.node_input(&log, NodeAddress { id: 0, n: 8 }).is_err());
    assert!(tree.node_input(&log, NodeAddress { id: 4, n: 4 }).is_err());
    assert!(tree.insert(NodeAddress::leaf(4), "again".into()).is_err());
    assert!(
        tree.insert(NodeAddress { id: 1, n: 2 }, "bad".into())
            .is_err()
    );
}

#[test]
fn zoom_opens_a_node_into_its_children_or_a_message_whole() {
    let log = small_log();
    let tree = built_tree(&log);
    assert_eq!(
        zoom(&tree, &log, 0, 2).unwrap(),
        "0+1|user: fix the bug\n1+1|assistant: looking\n"
    );
    assert_eq!(
        zoom(&tree, &log, 0, 4).unwrap(),
        format!(
            "0+2|{}\n2+2|{}\n",
            "user: fix the bug assistant: looking", "tool_call: read {} tool_result: ok"
        )
    );
    assert_eq!(zoom(&tree, &log, 2, 1).unwrap(), "tool_call: read {}");
}

#[test]
fn zoom_names_the_problem_for_a_bad_address() {
    let log = small_log();
    let mut tree = built_tree(&log);
    assert!(
        zoom(&tree, &log, 1, 2)
            .unwrap_err()
            .contains("multiple of n")
    );
    assert!(
        zoom(&tree, &log, 0, 3)
            .unwrap_err()
            .contains("power of two")
    );
    assert!(
        zoom(&tree, &log, 0, 0)
            .unwrap_err()
            .contains("power of two")
    );
    assert!(
        zoom(&tree, &log, 4, 1)
            .unwrap_err()
            .contains("outside the log")
    );
    assert!(
        zoom(&tree, &log, 0, 8)
            .unwrap_err()
            .contains("outside the log")
    );
    let mut sparse = SummaryTree::new();
    sparse.insert(NodeAddress::leaf(0), "x".into()).unwrap();
    assert!(
        zoom(&sparse, &log, 0, 2)
            .unwrap_err()
            .contains("1+1 is not built")
    );
    tree.insert(NodeAddress { id: 0, n: 8 }, "unrelated".into())
        .unwrap();
}

#[test]
fn date_returns_the_entry_timestamp_or_an_error() {
    let log = small_log();
    assert_eq!(date(&log, 2).unwrap(), "2026-10-27T00:00:02Z");
    assert!(date(&log, 4).unwrap_err().contains("outside the log"));
}

#[test]
fn prompts_carry_a_512_character_ruler_and_the_tool_definitions_name_both_tools() {
    let task = leaf_task(7, LogKind::ToolResult, "body");
    let ruler = task
        .lines()
        .find(|line| line.starts_with("---------|"))
        .expect("ruler line");
    assert_eq!(ruler.chars().count(), 512);
    assert!(task.contains("message 7") && task.contains("tool_result") && task.ends_with("body"));
    assert!(merge_task("a", "b", 4, 7).contains("messages 4 to 7"));
    assert!(too_long_task(600, "start").contains("600 bytes"));
    let names: Vec<String> = tool_definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(names, ["zoom", "date"]);
    assert!(SYSTEM_PROMPT.contains("zoom(id, n)"));
}
