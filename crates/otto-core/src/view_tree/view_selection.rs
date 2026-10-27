//! The view: the list of nodes that covers the log once, and its shrink rule.

use super::node_address::NodeAddress;
use super::summary_tree::{SummaryTree, render_line};

/// What one batch shrink changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrinkEvent {
    /// Index of the first line that differs from the view before the shrink.
    pub first_changed_line: usize,
    /// Number of lines before.
    pub lines_before: usize,
    /// Number of lines after.
    pub lines_after: usize,
    /// Rendered bytes before.
    pub bytes_before: usize,
    /// Rendered bytes after.
    pub bytes_after: usize,
    /// Rendered bytes of the lines before `first_changed_line`, which stay a prefix.
    pub unchanged_prefix_bytes: usize,
}

/// Nodes that cover messages `0..covered` once, oldest first.
///
/// Rendered size counts each built line as `id+n|text` plus a newline in UTF-8 bytes. A
/// line whose node is not built has no text and counts as 0 bytes.
#[derive(Debug, Clone)]
pub struct View {
    low_bytes: usize,
    high_bytes: usize,
    lines: Vec<NodeAddress>,
}

impl View {
    /// An empty view. A shrink runs when the size exceeds `high_bytes` and merges until it
    /// is at most `low_bytes`.
    pub fn new(low_bytes: usize, high_bytes: usize) -> Self {
        Self {
            low_bytes,
            high_bytes,
            lines: Vec::new(),
        }
    }

    /// Changes the shrink limits.
    pub fn set_limits(&mut self, low_bytes: usize, high_bytes: usize) {
        self.low_bytes = low_bytes;
        self.high_bytes = high_bytes;
    }

    /// Adds the next message as a leaf line. Panics if `leaf_id` is not the next id.
    pub fn append(&mut self, leaf_id: u64) {
        assert_eq!(
            leaf_id,
            self.covered(),
            "view append must be the next message id"
        );
        self.lines.push(NodeAddress::leaf(leaf_id));
    }

    /// Number of messages covered: the id the next append takes.
    pub fn covered(&self) -> u64 {
        self.lines.last().map_or(0, NodeAddress::end_id)
    }

    /// The lines, oldest first.
    pub fn lines(&self) -> &[NodeAddress] {
        &self.lines
    }

    /// Rendered size in bytes.
    pub fn bytes(&self, tree: &SummaryTree) -> usize {
        self.lines.iter().map(|line| line_bytes(tree, *line)).sum()
    }

    /// When the size exceeds the high limit, merges sibling pairs until it is at most the
    /// low limit or no pair can merge; returns what changed, or `None` if nothing merged.
    ///
    /// `t` is the number of log messages so far. A pair's due value is `(t - last) / n`
    /// where `last` is the last message id of the pair; the largest due merges first, a
    /// tie merges the older pair. Only a pair whose parent is built merges.
    pub fn shrink(&mut self, t: u64, tree: &SummaryTree) -> Option<ShrinkEvent> {
        let mut sizes: Vec<usize> = self
            .lines
            .iter()
            .map(|line| line_bytes(tree, *line))
            .collect();
        let bytes_before: usize = sizes.iter().sum();
        if bytes_before <= self.high_bytes {
            return None;
        }
        let lines_before = self.lines.len();
        let mut bytes = bytes_before;
        let mut first_changed: Option<usize> = None;
        while bytes > self.low_bytes {
            let Some(at) = self.most_due_pair(t, tree) else {
                break;
            };
            let parent = self.lines[at].parent();
            let merged = line_bytes(tree, parent);
            bytes = bytes - sizes[at] - sizes[at + 1] + merged;
            self.lines.splice(at..at + 2, [parent]);
            sizes.splice(at..at + 2, [merged]);
            first_changed = Some(first_changed.map_or(at, |first| first.min(at)));
        }
        let first_changed_line = first_changed?;
        Some(ShrinkEvent {
            first_changed_line,
            lines_before,
            lines_after: self.lines.len(),
            bytes_before,
            bytes_after: bytes,
            unchanged_prefix_bytes: sizes[..first_changed_line].iter().sum(),
        })
    }

    fn most_due_pair(&self, t: u64, tree: &SummaryTree) -> Option<usize> {
        let mut best: Option<(usize, u64, u64)> = None; // (index, t - last, n)
        for at in 0..self.lines.len().saturating_sub(1) {
            let (first, second) = (self.lines[at], self.lines[at + 1]);
            if !first.is_left() || !first.is_sibling_of(&second) || !tree.contains(first.parent()) {
                continue;
            }
            let age = t.saturating_sub(second.last_id());
            // due = age / n; compare age_a / n_a > age_b / n_b by cross-multiplication.
            // A tie keeps the earlier (older) pair.
            let better = best.is_none_or(|(_, best_age, best_n)| {
                u128::from(age) * u128::from(best_n) > u128::from(best_age) * u128::from(first.n)
            });
            if better {
                best = Some((at, age, first.n));
            }
        }
        best.map(|(at, _, _)| at)
    }

    /// The view as text: one `id+n|text` line per built node and a trailing newline.
    pub fn render(&self, tree: &SummaryTree) -> String {
        self.render_until(u64::MAX, tree)
    }

    /// Like [`View::render`] but only lines that end at or before message `last_id`, for the
    /// compressor's context. Unbuilt lines are left out.
    pub fn render_until(&self, last_id: u64, tree: &SummaryTree) -> String {
        let mut text = String::new();
        for line in self
            .lines
            .iter()
            .take_while(|line| line.last_id() <= last_id)
        {
            if let Some(node_text) = tree.get(*line) {
                text.push_str(&render_line(*line, node_text));
            }
        }
        text
    }
}

fn line_bytes(tree: &SummaryTree, address: NodeAddress) -> usize {
    // Turning '\n' into ' ' keeps the byte count, so the text length is used as is.
    tree.get(address)
        .map_or(0, |text| address.to_string().len() + 1 + text.len() + 1)
}
