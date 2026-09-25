//! The `/context` overlay: what the next provider request contains.
//!
//! It holds one [`ContextReport`] taken when the overlay opened; it does not
//! refresh while open. One section is expanded at a time, and an item's full
//! text opens in a scrollable view on top of the list.

use crossterm::event::KeyCode;
use otto_core::agent::context_report::{ContextReport, SectionKind};

use super::layout::format_token_count;

/// Width of a section's share bar at 100 percent.
const BAR_WIDTH: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row {
    Section(usize),
    Item(usize, usize),
}

/// An item's full text, scrolled by line.
#[derive(Debug, Clone)]
pub(crate) struct TextView {
    pub title: String,
    pub text: String,
    pub scroll: u16,
}

#[derive(Debug, Clone)]
pub(crate) struct ContextView {
    pub report: ContextReport,
    pub expanded: Option<usize>,
    pub selected: usize,
    pub text: Option<TextView>,
}

impl ContextView {
    pub fn new(report: ContextReport) -> Self {
        Self {
            report,
            expanded: None,
            selected: 0,
            text: None,
        }
    }

    /// The list rows: every section, and the expanded section's items under it.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (index, section) in self.report.sections.iter().enumerate() {
            rows.push(Row::Section(index));
            if self.expanded == Some(index) {
                rows.extend((0..section.items.len()).map(|item| Row::Item(index, item)));
            }
        }
        rows
    }

    /// Applies one key. Returns `false` when Esc closes the overlay.
    pub fn handle_key(&mut self, code: KeyCode) -> bool {
        if let Some(text) = &mut self.text {
            match code {
                KeyCode::Esc => self.text = None,
                KeyCode::Up => text.scroll = text.scroll.saturating_sub(1),
                KeyCode::Down => text.scroll = text.scroll.saturating_add(1),
                KeyCode::PageUp => text.scroll = text.scroll.saturating_sub(10),
                KeyCode::PageDown => text.scroll = text.scroll.saturating_add(10),
                _ => {}
            }
            return true;
        }
        match code {
            KeyCode::Esc => match self.expanded.take() {
                Some(section) => self.selected = section,
                None => return false,
            },
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Enter => match self.rows().get(self.selected) {
                Some(&Row::Section(section)) => {
                    self.expanded = (self.expanded != Some(section)).then_some(section);
                    self.selected = section;
                }
                Some(&Row::Item(section, item)) => {
                    let item = &self.report.sections[section].items[item];
                    self.text = Some(TextView {
                        title: format!(
                            "{} · ~{} tokens",
                            item.label,
                            format_token_count(item.tokens)
                        ),
                        text: item.text.clone(),
                        scroll: 0,
                    });
                }
                None => {}
            },
            _ => {}
        }
        true
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.rows().len() as isize;
        if len > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        }
    }

    /// The overlay title: model, estimate against the window, the last
    /// provider-reported input, and the compaction threshold.
    pub fn header(&self) -> String {
        let report = &self.report;
        let mut header = format!(
            "Context  {} · ~{}",
            report.model,
            format_token_count(report.estimated_total)
        );
        if report.context_window > 0 {
            header.push_str(&format!(" / {}", format_token_count(report.context_window)));
        }
        header.push_str(" tokens (estimate)");
        if let Some(reported) = report.reported_input_tokens {
            header.push_str(&format!(
                " · last reported {}",
                format_token_count(reported)
            ));
        }
        if report.compaction_threshold > 0 {
            header.push_str(&format!(
                " · compacts at {}",
                format_token_count(report.compaction_threshold)
            ));
        }
        header
    }

    /// One row's text: a section with its share bar, or an indented item.
    pub fn row_label(&self, row: Row) -> String {
        match row {
            Row::Section(index) => {
                let section = &self.report.sections[index];
                let total: i64 = self.report.sections.iter().map(|s| s.tokens.max(0)).sum();
                let bar = if total > 0 {
                    section.tokens.max(0) as usize * BAR_WIDTH / total as usize
                } else {
                    0
                };
                format!(
                    "{:<28} {:>7}  {}",
                    section_name(section.kind, section.items.len()),
                    format_token_count(section.tokens),
                    "█".repeat(bar)
                )
            }
            Row::Item(section, item) => {
                let item = &self.report.sections[section].items[item];
                format!(
                    "    {:<24} {:>7}",
                    item.label,
                    format_token_count(item.tokens)
                )
            }
        }
    }
}

fn section_name(kind: SectionKind, items: usize) -> String {
    match kind {
        SectionKind::SystemPrompt => "System prompt".into(),
        SectionKind::Tools => format!("Tools (built-in, {items})"),
        SectionKind::McpTools => format!("Tools (MCP, {items})"),
        SectionKind::CompactionSummary => "Compaction summary".into(),
        SectionKind::Memory => "Memory (last turn)".into(),
        SectionKind::Messages => format!("Messages ({items})"),
    }
}

#[cfg(test)]
mod tests {
    use otto_core::agent::context_report::{ContextItem, ContextSection};

    use super::*;

    fn item(label: &str, tokens: i64) -> ContextItem {
        ContextItem {
            label: label.into(),
            tokens,
            text: format!("text of {label}\nsecond line"),
        }
    }

    fn view() -> ContextView {
        ContextView::new(ContextReport {
            model: "gpt-5".into(),
            context_window: 272_000,
            compaction_threshold: 200_000,
            estimated_total: 41_200,
            reported_input_tokens: Some(39_800),
            sections: vec![
                ContextSection {
                    kind: SectionKind::SystemPrompt,
                    tokens: 6_100,
                    items: vec![item("Base", 1_200), item("Workspace instructions", 4_900)],
                },
                ContextSection {
                    kind: SectionKind::Messages,
                    tokens: 35_100,
                    items: vec![item("#1 user", 100), item("#2 assistant", 35_000)],
                },
            ],
        })
    }

    #[test]
    fn the_header_names_the_estimate_window_reported_input_and_threshold() {
        assert_eq!(
            view().header(),
            "Context  gpt-5 · ~41.2k / 272k tokens (estimate) · last reported 39.8k · compacts at 200k"
        );
    }

    #[test]
    fn a_section_row_shows_its_tokens_and_share() {
        let view = view();
        let label = view.row_label(Row::Section(1));
        assert!(label.starts_with("Messages (2)"), "{label}");
        assert!(label.contains("35.1k"), "{label}");
        assert_eq!(label.matches('█').count(), 17, "{label}");
    }

    #[test]
    fn enter_expands_a_section_opens_an_item_and_esc_goes_back() {
        let mut view = view();
        assert_eq!(view.rows(), vec![Row::Section(0), Row::Section(1)]);

        view.handle_key(KeyCode::Down);
        assert!(view.handle_key(KeyCode::Enter));
        assert_eq!(
            view.rows(),
            vec![
                Row::Section(0),
                Row::Section(1),
                Row::Item(1, 0),
                Row::Item(1, 1)
            ]
        );

        view.handle_key(KeyCode::Down);
        view.handle_key(KeyCode::Down);
        view.handle_key(KeyCode::Enter);
        let text = view.text.as_ref().expect("the item text is open");
        assert!(text.title.starts_with("#2 assistant"), "{}", text.title);
        assert_eq!(text.text, "text of #2 assistant\nsecond line");

        view.handle_key(KeyCode::Down);
        assert_eq!(view.text.as_ref().expect("still open").scroll, 1);

        assert!(view.handle_key(KeyCode::Esc));
        assert!(view.text.is_none());
        assert!(view.handle_key(KeyCode::Esc));
        assert_eq!(view.expanded, None);
        assert_eq!(view.selected, 1, "the collapsed section stays selected");
        assert!(!view.handle_key(KeyCode::Esc), "Esc on the list closes it");
    }
}
