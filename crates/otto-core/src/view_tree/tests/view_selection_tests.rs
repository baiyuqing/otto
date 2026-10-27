//! View shrink rule and build order.

use crate::view_tree::*;

/// Taelin's push list: (keep, state, older) with life always 0.
enum Pushed {
    Empty,
    Node {
        keep: bool,
        state: u64,
        older: Box<Pushed>,
    },
}

fn push(new_state: u64, states: Pushed) -> Pushed {
    match states {
        Pushed::Empty => Pushed::Node {
            keep: false,
            state: new_state,
            older: Box::new(Pushed::Empty),
        },
        Pushed::Node {
            keep: false,
            state,
            older,
        } => Pushed::Node {
            keep: true,
            state,
            older,
        },
        Pushed::Node {
            keep: true,
            state,
            older,
        } => Pushed::Node {
            keep: false,
            state: new_state,
            older: Box::new(push(state, *older)),
        },
    }
}

fn push_view(states: &Pushed, total: u64) -> Vec<NodeAddress> {
    let mut starts = Vec::new();
    let mut at = states;
    while let Pushed::Node { state, older, .. } = at {
        starts.push(*state);
        at = older;
    }
    starts.sort_unstable();
    let ends: Vec<u64> = starts[1..].iter().copied().chain([total]).collect();
    starts
        .iter()
        .zip(ends)
        .map(|(start, end)| NodeAddress {
            id: *start,
            n: end - *start,
        })
        .collect()
}

/// Every rendered line is `LINE_BYTES` long, so a byte budget is a line budget.
const LINE_BYTES: usize = 64;

fn uniform_tree(log_length: u64) -> SummaryTree {
    let mut tree = SummaryTree::new();
    for address in SummaryTree::addresses_for_log(log_length) {
        let text_length = LINE_BYTES - 2 - address.to_string().len();
        tree.insert(address, "x".repeat(text_length)).unwrap();
    }
    tree
}

#[test]
fn the_view_equals_the_push_list_for_every_t_up_to_2000() {
    let tree = uniform_tree(2000);
    let mut states = Pushed::Empty;
    let mut view = View::new(0, 0);
    for t in 0..2000u64 {
        states = push(t, states);
        let target = push_view(&states, t + 1);
        view.append(t);
        view.set_limits(LINE_BYTES * target.len(), LINE_BYTES * target.len());
        view.shrink(t + 1, &tree);
        assert_eq!(view.lines(), target.as_slice(), "t = {t}");
    }
}

#[test]
fn at_t_10_the_newest_pair_merges_before_the_older_larger_pair() {
    let tree = uniform_tree(16);
    let mut states = Pushed::Empty;
    let mut view = View::new(0, 0);
    for t in 0..9u64 {
        states = push(t, states);
        let target = push_view(&states, t + 1);
        view.append(t);
        view.set_limits(LINE_BYTES * target.len(), LINE_BYTES * target.len());
        view.shrink(t + 1, &tree);
    }
    let lines = |pairs: &[(u64, u64)]| -> Vec<NodeAddress> {
        pairs
            .iter()
            .map(|(id, n)| NodeAddress { id: *id, n: *n })
            .collect()
    };
    assert_eq!(view.lines(), lines(&[(0, 4), (4, 4), (8, 1)]));
    view.append(9);
    view.set_limits(3 * LINE_BYTES, 3 * LINE_BYTES);
    let event = view.shrink(10, &tree).expect("one merge");
    // 8+1 & 9+1 is due (10-9)/1 = 1; 0+4 & 4+4 is due (10-7)/4 = 0.75.
    assert_eq!(view.lines(), lines(&[(0, 4), (4, 4), (8, 2)]));
    assert_eq!(event.first_changed_line, 2);
    assert_eq!(event.unchanged_prefix_bytes, 2 * LINE_BYTES);
    assert_eq!((event.lines_before, event.lines_after), (4, 3));
    assert_eq!(
        (event.bytes_before, event.bytes_after),
        (4 * LINE_BYTES, 3 * LINE_BYTES)
    );
}

#[test]
fn shrink_never_merges_a_pair_whose_parent_is_unbuilt() {
    let mut tree = SummaryTree::new();
    for id in 0..8 {
        tree.insert(NodeAddress::leaf(id), "leaf".into()).unwrap();
    }
    tree.insert(NodeAddress { id: 0, n: 2 }, "pair".into())
        .unwrap();
    let mut view = View::new(0, 0);
    for id in 0..8 {
        view.append(id);
    }
    let event = view.shrink(8, &tree).expect("the built parent merges");
    assert_eq!(view.lines().len(), 7);
    assert_eq!(view.lines()[0], NodeAddress { id: 0, n: 2 });
    assert_eq!(event.first_changed_line, 0);
    assert_eq!(event.unchanged_prefix_bytes, 0);
    assert!(
        view.shrink(8, &tree).is_none(),
        "no pair with a built parent is left"
    );
}

#[test]
fn shrink_does_nothing_at_or_below_the_high_limit() {
    let tree = uniform_tree(4);
    let mut view = View::new(0, 4 * LINE_BYTES);
    for id in 0..4 {
        view.append(id);
    }
    assert!(view.shrink(4, &tree).is_none());
    assert_eq!(view.lines().len(), 4);
}

#[test]
fn render_lines_flatten_newlines_and_render_until_stops_at_the_last_id() {
    let mut tree = SummaryTree::new();
    tree.insert(NodeAddress::leaf(0), "a\nb".into()).unwrap();
    tree.insert(NodeAddress::leaf(1), "c".into()).unwrap();
    tree.insert(NodeAddress { id: 0, n: 2 }, "ab\nc".into())
        .unwrap();
    let mut view = View::new(0, 0);
    view.append(0);
    view.append(1);
    assert_eq!(view.render(&tree), "0+1|a b\n1+1|c\n");
    assert_eq!(view.render_until(0, &tree), "0+1|a b\n");
    assert_eq!(view.bytes(&tree), "0+1|a b\n1+1|c\n".len());
    view.shrink(2, &tree);
    assert_eq!(view.render(&tree), "0+2|ab c\n");
    assert_eq!(view.render_until(0, &tree), "");
}

#[test]
fn schedule_releases_leaves_in_a_window_and_merges_when_both_children_are_built() {
    let leaf = NodeAddress::leaf;
    let pair = |id| NodeAddress { id, n: 2 };
    let mut schedule = BuildSchedule::new(4, 2);
    assert_eq!(schedule.take_ready(), [leaf(0), leaf(1)]);
    assert!(schedule.take_ready().is_empty());
    schedule.complete(leaf(1)).unwrap();
    assert_eq!(schedule.take_ready(), [leaf(2)]);
    schedule.complete(leaf(0)).unwrap();
    assert_eq!(schedule.take_ready(), [leaf(3), pair(0)]);
    schedule.complete(leaf(3)).unwrap();
    schedule.complete(leaf(2)).unwrap();
    assert_eq!(schedule.take_ready(), [pair(2)]);
    assert!(!schedule.is_done());
    schedule.complete(pair(0)).unwrap();
    schedule.complete(pair(2)).unwrap();
    assert_eq!(schedule.take_ready(), [NodeAddress { id: 0, n: 4 }]);
    schedule.complete(NodeAddress { id: 0, n: 4 }).unwrap();
    assert!(schedule.is_done());
    assert!(schedule.complete(leaf(0)).is_err());
}

#[test]
fn schedule_skips_nodes_completed_before_they_were_handed_out() {
    let mut schedule = BuildSchedule::new(2, 8);
    schedule.complete(NodeAddress::leaf(0)).unwrap();
    schedule.complete(NodeAddress { id: 0, n: 2 }).unwrap();
    // Leaf 1 is still ready; leaf 0 and the finished parent are not handed out again.
    assert_eq!(schedule.take_ready(), [NodeAddress::leaf(1)]);
    schedule.complete(NodeAddress::leaf(1)).unwrap();
    assert!(schedule.take_ready().is_empty());
    assert!(schedule.is_done());
}
