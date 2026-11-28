//! The `/context` panel: what the provider request contains and in which order.
//!
//! The panel shows one basis (the last request that was sent, or the projected
//! next request) in one layout (grouped by section, or one row per item in
//! request order), or the result of a text search over the items of that basis.

use chrono::{Local, TimeZone};
use otto_core::agent::context_report::{ContextItem, ContextReport, ContextSection, SectionKind};

use super::{Document, Presenter, Transition};
use crate::tui::backend::{Reply, Request};
use crate::tui::interaction::{Action, Activation, Row, View};
use crate::tui::layout::format_token_count;

/// Characters of an item's text shown for a search match.
const EXCERPT_CHARS: usize = 80;
/// Characters before the match that the excerpt starts at.
const EXCERPT_LEAD_CHARS: usize = 24;

/// Which request the sections come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Basis {
    /// The last ordinary request handed to the provider.
    LastSent,
    /// The projected next request.
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// One row per section; `expanded` lists the items of one section.
    BySection { expanded: Option<usize> },
    /// One row per item that has a position, sorted by position.
    RequestOrder,
}

/// Browsing and searching exclude each other; a search remembers the layout
/// it was started from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Browse(Layout),
    Search { query: String, from: Layout },
}

pub(crate) struct Context {
    report: Option<ContextReport>,
    basis: Basis,
    mode: Mode,
}

/// An item of the current basis with its place in the sections.
struct Entry<'a> {
    section: usize,
    index: usize,
    kind: SectionKind,
    item: &'a ContextItem,
}

impl Context {
    pub(crate) fn new() -> Self {
        Self {
            report: None,
            basis: Basis::Next,
            mode: Mode::Browse(Layout::BySection { expanded: None }),
        }
    }

    fn sections(&self) -> &[ContextSection] {
        let Some(report) = &self.report else {
            return &[];
        };
        match (self.basis, &report.last_sent) {
            (Basis::LastSent, Some(sent)) => &sent.sections,
            _ => &report.sections,
        }
    }

    /// The items of the basis in request order. An item without a position
    /// comes last; ties keep section order.
    fn entries(&self) -> Vec<Entry<'_>> {
        let mut entries: Vec<Entry<'_>> = self
            .sections()
            .iter()
            .enumerate()
            .flat_map(|(section, group)| {
                group
                    .items
                    .iter()
                    .enumerate()
                    .map(move |(index, item)| Entry {
                        section,
                        index,
                        kind: group.kind,
                        item,
                    })
            })
            .collect();
        entries.sort_by_key(|entry| (entry.item.position.is_none(), entry.item.position));
        entries
    }

    fn title(report: &ContextReport) -> String {
        let mut title = if report.estimate_complete {
            format!(
                "Context  {} · ~{}",
                report.model,
                format_token_count(report.estimated_total)
            )
        } else {
            format!("Context  {} · estimate unknown", report.model)
        };
        if report.context_window > 0 {
            title.push_str(&format!(" / {}", format_token_count(report.context_window)));
        }
        if report.estimate_complete {
            title.push_str(" tokens (estimate)");
        }
        if let Some(tokens) = report.reported_input_tokens {
            title.push_str(&format!(" · last reported {}", format_token_count(tokens)));
        }
        if report.compaction_threshold > 0 {
            title.push_str(&format!(
                " · compacts at {}",
                format_token_count(report.compaction_threshold)
            ));
        }
        title
    }

    fn notice(&self, report: &ContextReport) -> String {
        match (self.basis, &report.last_sent) {
            (Basis::LastSent, Some(sent)) => {
                let tokens: i64 = sent.sections.iter().map(|s| s.tokens.max(0)).sum();
                format!(
                    "Last sent request · {} · ~{} tokens (estimate)",
                    clock(sent.sent_at),
                    format_token_count(tokens)
                )
            }
            _ => "Next request (projected; no recall for the next turn yet)".into(),
        }
    }

    fn actions(&self, report: &ContextReport) -> Vec<Action> {
        let has_sent = report.last_sent.is_some();
        vec![
            Action {
                id: "basis".into(),
                label: match self.basis {
                    Basis::LastSent => "Basis: last sent (r)".into(),
                    Basis::Next => "Basis: next request (r)".into(),
                },
                enabled: has_sent,
                reason: (!has_sent).then(|| "No request has been sent yet".into()),
                shortcut: Some('r'),
            },
            Action {
                id: "order".into(),
                label: match self.mode {
                    Mode::Browse(Layout::RequestOrder) => "Layout: request order (o)".into(),
                    _ => "Layout: by section (o)".into(),
                },
                enabled: true,
                shortcut: Some('o'),
                ..Action::default()
            },
            Action {
                id: "search".into(),
                label: "Search (/)".into(),
                enabled: true,
                shortcut: Some('/'),
                ..Action::default()
            },
        ]
    }

    fn section_rows(&self, expanded: Option<usize>) -> Vec<Row> {
        let sections = self.sections();
        let total: i64 = sections.iter().map(|s| s.tokens.max(0)).sum();
        let mut rows = Vec::new();
        for (si, section) in sections.iter().enumerate() {
            let share_percent = if total > 0 {
                (section.tokens.max(0) as usize * 100 / total as usize).min(100)
            } else {
                0
            };
            rows.push(Row {
                id: serde_json::to_string(&("section", si, None::<usize>)).unwrap(),
                cells: vec![
                    section_name(section.kind, section.items.len(), self.basis),
                    format_token_count(section.tokens),
                    format!("{share_percent}%"),
                ],
                detail: String::new(),
            });
            if expanded == Some(si) {
                for (ii, item) in section.items.iter().enumerate() {
                    rows.push(Row {
                        id: item_id(si, ii),
                        cells: vec![item.label.clone(), format_token_count(item.tokens)],
                        detail: String::new(),
                    });
                }
            }
        }
        rows
    }

    /// Every item in request order with the estimated tokens after it. `None`
    /// for an item that is not part of the request.
    fn order_table(&self) -> Vec<(Entry<'_>, Option<i64>)> {
        let entries = self.entries();
        let mut to_end = vec![None; entries.len()];
        let mut running = 0_i64;
        let mut end = entries.len();
        while end > 0 {
            let position = entries[end - 1].item.position;
            let mut start = end;
            while start > 0 && entries[start - 1].item.position == position {
                start -= 1;
            }
            if position.is_some() {
                for slot in &mut to_end[start..end] {
                    *slot = Some(running);
                }
                for entry in &entries[start..end] {
                    running = running.saturating_add(entry.item.tokens.max(0));
                }
            }
            end = start;
        }
        entries.into_iter().zip(to_end).collect()
    }

    fn order_row(entry: &Entry<'_>, to_end: Option<i64>, detail: String) -> Row {
        Row {
            id: item_id(entry.section, entry.index),
            cells: vec![
                entry
                    .item
                    .position
                    .map_or_else(|| "—".into(), |position| (position + 1).to_string()),
                item_name(entry.kind, &entry.item.label),
                format_token_count(entry.item.tokens),
                to_end.map_or_else(|| "—".into(), format_token_count),
            ],
            detail,
        }
    }

    fn order_columns() -> Vec<String> {
        ["#", "Item", "Tokens", "To end"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn search_view(&self, query: &str) -> View {
        let needle = query.to_ascii_lowercase();
        let rows: Vec<Row> = if needle.is_empty() {
            Vec::new()
        } else {
            self.order_table()
                .iter()
                .filter_map(|(entry, to_end)| {
                    let text = entry.item.text.to_ascii_lowercase();
                    let in_text = text.find(&needle);
                    (in_text.is_some() || entry.item.label.to_ascii_lowercase().contains(&needle))
                        .then(|| {
                            let detail = excerpt(&entry.item.text, in_text);
                            Self::order_row(entry, *to_end, detail)
                        })
                })
                .collect()
        };
        let document = if needle.is_empty() {
            "Type to search the labels and text of the request items. Matches are listed in \
             request order."
                .into()
        } else if rows.is_empty() {
            format!("No item contains \"{query}\".")
        } else {
            String::new()
        };
        View {
            description: format!(
                "Search: {query} · {} matches · Backspace on an empty search leaves it",
                rows.len()
            ),
            columns: Self::order_columns(),
            rows,
            document,
            search: Some(query.to_owned()),
            ..View::default()
        }
    }
}

impl Presenter for Context {
    fn key(&self) -> String {
        "context".into()
    }

    fn view(&self) -> View {
        let Some(report) = &self.report else {
            return View {
                collection: true,
                title: "Context".into(),
                description: "Loading context…".into(),
                loading: true,
                ..View::default()
            };
        };
        let frame = View {
            collection: true,
            title: Self::title(report),
            notice: Some(self.notice(report)),
            ..View::default()
        };
        match &self.mode {
            Mode::Browse(Layout::BySection { expanded }) => View {
                rows: self.section_rows(*expanded),
                actions: self.actions(report),
                ..frame
            },
            Mode::Browse(Layout::RequestOrder) => View {
                description: "Request order · To end: estimated tokens after the item".into(),
                columns: Self::order_columns(),
                rows: self
                    .order_table()
                    .iter()
                    .map(|(entry, to_end)| Self::order_row(entry, *to_end, String::new()))
                    .collect(),
                actions: self.actions(report),
                ..frame
            },
            Mode::Search { query, .. } => View {
                title: frame.title,
                notice: frame.notice,
                collection: true,
                ..self.search_view(query)
            },
        }
    }

    fn activate(&mut self, activation: Activation) -> Transition {
        match activation {
            Activation::Action { id, .. } if id == "basis" => {
                let has_sent = self.report.as_ref().is_some_and(|r| r.last_sent.is_some());
                if has_sent && matches!(self.mode, Mode::Browse(_)) {
                    self.basis = match self.basis {
                        Basis::LastSent => Basis::Next,
                        Basis::Next => Basis::LastSent,
                    };
                    // Section indexes belong to one basis.
                    if let Mode::Browse(Layout::BySection { expanded }) = &mut self.mode {
                        *expanded = None;
                    }
                }
                Transition::Stay
            }
            Activation::Action { id, .. } if id == "order" => {
                if let Mode::Browse(layout) = &self.mode {
                    self.mode = Mode::Browse(match layout {
                        Layout::BySection { .. } => Layout::RequestOrder,
                        Layout::RequestOrder => Layout::BySection { expanded: None },
                    });
                }
                Transition::Stay
            }
            Activation::Action { id, .. } if id == "search" => {
                if let Mode::Browse(layout) = self.mode {
                    self.mode = Mode::Search {
                        query: String::new(),
                        from: layout,
                    };
                }
                Transition::Stay
            }
            Activation::Action { id, fields, .. } if id == "otto:search" => {
                if let Mode::Search { query, from } = &mut self.mode {
                    let next = fields.get("query").cloned().unwrap_or_default();
                    if next.is_empty() && query.is_empty() {
                        // Backspace on an empty query.
                        self.mode = Mode::Browse(*from);
                    } else {
                        *query = next;
                    }
                }
                Transition::Stay
            }
            Activation::Open(id) => self.open(&id),
            _ => Transition::Stay,
        }
    }

    fn receive(&mut self, reply: Reply) -> Transition {
        if let Reply::Context(report) = reply {
            self.basis = if report.last_sent.is_some() {
                Basis::LastSent
            } else {
                Basis::Next
            };
            self.report = Some(report);
        }
        Transition::Stay
    }

    fn refresh(&self) -> Option<Request> {
        self.report.is_none().then_some(Request::Context)
    }
}

impl Context {
    fn open(&mut self, id: &str) -> Transition {
        let Ok(key) = serde_json::from_str::<(String, usize, Option<usize>)>(id) else {
            return Transition::Stay;
        };
        if key.0 == "section" {
            if let Mode::Browse(Layout::BySection { expanded }) = &mut self.mode {
                *expanded = (*expanded != Some(key.1)).then_some(key.1);
            }
            return Transition::Stay;
        }
        let Some(item) = key
            .2
            .and_then(|ii| self.sections().get(key.1)?.items.get(ii))
        else {
            return Transition::Stay;
        };
        Transition::Push(Box::new(Document {
            key: format!("context:{id}"),
            title: format!(
                "{} · ~{} tokens",
                item.label,
                format_token_count(item.tokens)
            ),
            text: item.text.clone(),
        }))
    }
}

fn item_id(section: usize, index: usize) -> String {
    serde_json::to_string(&("item", section, Some(index))).unwrap()
}

/// The label with a short section name where the label alone is ambiguous.
fn item_name(kind: SectionKind, label: &str) -> String {
    match kind {
        SectionKind::SystemPrompt => format!("system {label}"),
        SectionKind::Tools | SectionKind::McpTools => format!("tool {label}"),
        SectionKind::CompactionSummary => format!("summary {label}"),
        SectionKind::Memory | SectionKind::Messages => label.to_owned(),
    }
}

fn section_name(kind: SectionKind, count: usize, basis: Basis) -> String {
    match kind {
        SectionKind::SystemPrompt => "System prompt".into(),
        SectionKind::Tools => format!("Tools (built-in, {count})"),
        SectionKind::McpTools => format!("Tools (MCP, {count})"),
        SectionKind::CompactionSummary => "Compaction summary".into(),
        SectionKind::Memory => match basis {
            Basis::LastSent => "Memory recall".into(),
            Basis::Next => "Memory (last turn)".into(),
        },
        SectionKind::Messages => format!("Messages ({count})"),
    }
}

/// Local time of day of a Unix-millisecond timestamp.
fn clock(unix_ms: i64) -> String {
    Local.timestamp_millis_opt(unix_ms).single().map_or_else(
        || "unknown time".into(),
        |time| time.format("%H:%M:%S").to_string(),
    )
}

/// One line of about [`EXCERPT_CHARS`] characters around the match that starts
/// at byte `found` of the ASCII-lowercased text, or from the start of `text`
/// when only the label matched. Newlines become spaces.
fn excerpt(text: &str, found: Option<usize>) -> String {
    // ASCII lowercasing keeps every byte offset, so `found` is a char boundary.
    let lead = found.map_or(0, |at| {
        text[..at]
            .chars()
            .count()
            .saturating_sub(EXCERPT_LEAD_CHARS)
    });
    let mut chars = text.chars().skip(lead);
    let body: String = chars
        .by_ref()
        .take(EXCERPT_CHARS)
        .map(|ch| if matches!(ch, '\n' | '\r') { ' ' } else { ch })
        .collect();
    let before = if lead > 0 { "…" } else { "" };
    let after = if chars.next().is_some() { "…" } else { "" };
    format!("{before}{body}{after}")
}
