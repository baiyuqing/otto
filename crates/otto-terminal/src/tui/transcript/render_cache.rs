//! Per-entry rendering cache for the transcript.
//!
//! [`super::lines`] renders every entry on every call: markdown parsing,
//! wrapping and tool-argument JSON parsing cost about 0.19 ms per 2.3 KB
//! assistant entry. The TUI redraws on a timer, so [`RenderCache`] keeps each
//! entry's rows and re-renders an entry only when it differs from the copy
//! the rows were rendered from.
//!
//! Validity is index-aligned and keyed by entry equality, so the code that
//! mutates entries needs no version counter: entry `i` is reused when the
//! cached source equals `entries[i]` and the shared width and details flag are
//! unchanged. Rows produced by [`RenderCache::rows`] are identical to the
//! matching slice of [`super::lines`].

use std::ops::Range;

use ratatui::text::Line;

use super::entry_lines;
use crate::tui::entries::Entry;

struct CachedEntry {
    /// The entry the rows were rendered from; compared with the live entry.
    source: Entry,
    /// Rows of this entry alone, without separator lines.
    lines: Vec<Line<'static>>,
}

/// Rendered rows of a transcript, kept between frames.
#[derive(Default)]
pub(crate) struct RenderCache {
    width: usize,
    details: bool,
    entries: Vec<CachedEntry>,
    /// Rows of the whole transcript including separators, set by `sync`.
    total_rows: usize,
    /// Number of `entry_lines` calls made by `sync`; read by tests only.
    #[cfg(test)]
    rendered_count: usize,
}

/// A running tool call renders its elapsed time from the clock
/// (`Entry::execution_elapsed`), so its rows change between frames while the
/// entry itself stays equal. Such an entry is re-rendered on every `sync`.
fn reads_clock(entry: &Entry) -> bool {
    entry.tool_started.is_some() && entry.tool_elapsed.is_none()
}

impl RenderCache {
    /// Re-renders only the entries whose source, or the shared width/details,
    /// changed, and drops cached entries beyond `entries.len()`.
    pub(crate) fn sync(&mut self, entries: &[Entry], details: bool, width: usize) {
        if self.width != width || self.details != details {
            self.entries.clear();
            self.width = width;
            self.details = details;
        }
        self.entries.truncate(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let reusable = self
                .entries
                .get(index)
                .is_some_and(|cached| !reads_clock(entry) && cached.source == *entry);
            if reusable {
                continue;
            }
            #[cfg(test)]
            {
                self.rendered_count += 1;
            }
            let rendered = CachedEntry {
                source: entry.clone(),
                lines: entry_lines(entry, details, width),
            };
            match self.entries.get_mut(index) {
                Some(slot) => *slot = rendered,
                None => self.entries.push(rendered),
            }
        }
        let rendered_entries = self
            .entries
            .iter()
            .filter(|cached| !cached.lines.is_empty());
        let (entry_rows, entry_blocks) = rendered_entries.fold((0, 0), |(rows, blocks), cached| {
            (rows + cached.lines.len(), blocks + 1)
        });
        // One blank separator between each pair of consecutive non-empty entries.
        self.total_rows = entry_rows + usize::saturating_sub(entry_blocks, 1);
    }

    /// Total rows, equal to `super::lines(entries, details, width).len()` for
    /// the inputs of the last `sync`.
    pub(crate) fn row_count(&self) -> usize {
        self.total_rows
    }

    /// Rows `range`, clamped to `row_count`, exactly as [`super::lines`] would
    /// produce them, blank separator rows included. Only rows inside the
    /// range are cloned.
    pub(crate) fn rows(&self, range: Range<usize>) -> Vec<Line<'static>> {
        let end = range.end.min(self.total_rows);
        let start = range.start;
        let mut window = Vec::new();
        if start >= end {
            return window;
        }
        let mut position = 0;
        let mut seen_block = false;
        for cached in self
            .entries
            .iter()
            .filter(|cached| !cached.lines.is_empty())
        {
            if seen_block {
                if (start..end).contains(&position) {
                    window.push(Line::default());
                }
                position += 1;
            }
            seen_block = true;
            let block_end = position + cached.lines.len();
            if block_end > start && position < end {
                let from = start.saturating_sub(position);
                let to = end.min(block_end) - position;
                window.extend_from_slice(&cached.lines[from..to]);
            }
            position = block_end;
            if position >= end {
                break;
            }
        }
        window
    }

    /// Number of entry renders `sync` has performed so far.
    #[cfg(test)]
    pub(crate) fn rendered_count(&self) -> usize {
        self.rendered_count
    }
}

#[cfg(test)]
mod tests;
