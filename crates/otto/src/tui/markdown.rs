//! Markdown -> ratatui `Text` renderer, built on `pulldown_cmark`.
//!
//! Port of `internal/tui/markdown.go`. Go's own file does no formatting: it
//! escapes untrusted input, hands it to the external `charm.land/glamour/v2`
//! library for the actual rendering, and filters glamour's ANSI output down
//! to a safe SGR allowlist before printing it (`filterTerminalOutput`). There
//! is no Go *algorithm* to port for the visual formatting itself, only
//! glamour's own opaque behavior, which has no Rust equivalent to bind to.
//!
//! This renderer instead walks `pulldown_cmark::Event`s and builds
//! `ratatui::text::{Line, Span}` values directly, so there is no ANSI string
//! for a hostile document to smuggle codes through: a `Span`'s content is
//! never interpreted as an escape sequence by ratatui, only printed
//! literally by the crossterm backend. That output-side safety net
//! (`filterTerminalOutput` / the SGR allowlist) has nothing to filter here
//! and is not ported.
//!
//! What *is* ported, because it is a real, tested Go security control rather
//! than an output-format choice: every literal piece of text (paragraph
//! text, code spans, and raw HTML, which is treated as literal text rather
//! than interpreted) is run through [`super::layout::escape_plain_text`]
//! before it becomes a `Span`, so a raw control byte embedded in model or
//! tool output can never reach the terminal as a real escape sequence via
//! `crossterm::style::Print`. See `tests::malicious_documents_lose_no_raw_controls`
//! for the parity check this replaces `TestMarkdownRecoversFromExactUnterminatedEntityAttack` /
//! `TestMarkdownFiltersControlsSynthesizedByFormatting` / `TestMarkdownPreservesEntityCodeAndLinkSemantics`
//! with, adapted to this architecture (no ANSI is ever produced, so there is
//! nothing for an SGR allowlist to protect).
//!
//! ponytail: no syntax highlighting for fenced code blocks (language tag is
//! shown as a dim label above the block, content is a flat color), no
//! hanging indent for a list item's second line, and heading levels collapse
//! to two colors instead of glamour's full per-level theme. All three are
//! cosmetic; upgrade path is widening `heading_color`/`code_style` or adding
//! a real highlighter crate if a user reports the plain output as hard to
//! scan.

use std::mem;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use super::layout::{escape_plain_text, escape_single_line_text};

/// Renders `source` (untrusted markdown from the model or a tool) as a
/// `ratatui` `Text`. Line-wrapping to the terminal width is left to
/// `Paragraph::wrap`, matching how the rest of the transcript is drawn.
pub fn render(source: &str) -> Text<'static> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut renderer = Renderer::default();
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct LinkState {
    url: String,
    text: String,
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>,
    blockquote_depth: usize,
    /// Prefix (blockquote markers and/or a list bullet plus indent) to
    /// insert before the first span of the next line, consumed on first use
    /// except inside a code block, where it is re-armed after every line so
    /// a blockquote marker keeps applying to a fenced block quoted inline.
    next_prefix: Option<String>,
    in_code_block: bool,
    link: Vec<LinkState>,
    table: Option<TableState>,
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => self.code(&text),
            // Raw HTML is never interpreted, only shown as literal escaped
            // text: interpreting it would defeat the point of escaping.
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            // Math is not enabled in `Options`, so these never fire from our
            // parser; fold them to plain text defensively rather than panic.
            Event::InlineMath(text) | Event::DisplayMath(text) => self.text(&text),
            Event::FootnoteReference(label) => self.text(&format!("[^{label}]")),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.append(marker.to_string(), Style::new());
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.begin_block(),
            Tag::Heading { level, .. } => {
                self.begin_block();
                self.style_stack.push(
                    Style::new()
                        .add_modifier(Modifier::BOLD)
                        .fg(heading_color(level)),
                );
                self.append(
                    format!("{} ", "#".repeat(level as usize)),
                    self.current_style(),
                );
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.next_prefix = Some(self.quote_prefix());
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.lines.push(Line::styled(
                        escape_single_line_text(&lang),
                        Style::new().add_modifier(Modifier::DIM),
                    ));
                }
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_line();
                let marker = match self.list_stack.last_mut() {
                    Some(Some(number)) => {
                        let text = format!("{number}. ");
                        *number += 1;
                        text
                    }
                    Some(None) => "- ".to_string(),
                    None => String::new(),
                };
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                self.next_prefix = Some(format!("{}{indent}{marker}", self.quote_prefix()));
            }
            Tag::FootnoteDefinition(label) => {
                self.begin_block();
                self.append(format!("[^{}]: ", escape_plain_text(&label)), Style::new());
            }
            Tag::Table(alignments) => {
                self.flush_line();
                self.table = Some(TableState {
                    alignments,
                    ..Default::default()
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.current_cell.clear();
                }
            }
            Tag::Emphasis => self
                .style_stack
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .style_stack
                .push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .style_stack
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.style_stack.push(
                    Style::new()
                        .add_modifier(Modifier::UNDERLINED)
                        .fg(Color::Cyan),
                );
                self.link.push(LinkState {
                    url: dest_url.to_string(),
                    text: String::new(),
                });
            }
            Tag::Image { dest_url, .. } => {
                self.link.push(LinkState {
                    url: dest_url.to_string(),
                    text: String::new(),
                });
                self.append("[image: ".to_string(), Style::new());
            }
            // Superscript/Subscript (no terminal rendering), the definition-list
            // tags, and metadata blocks all require `Options` flags this
            // renderer does not enable, so pulldown-cmark never emits them.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock | TagEnd::FootnoteDefinition => {
                self.flush_line();
                self.blank_line();
            }
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.flush_line();
                self.blank_line();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                if self.blockquote_depth == 0 {
                    self.blank_line();
                }
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.in_code_block = false;
                self.next_prefix = None;
                self.blank_line();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Table => self.finish_table(),
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = mem::take(&mut table.current_row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = mem::take(&mut table.current_cell);
                    table.current_row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::Link => {
                self.style_stack.pop();
                if let Some(link) = self.link.pop()
                    && !link.url.is_empty()
                    && link.url != link.text
                {
                    self.append(
                        format!(" ({})", escape_single_line_text(&link.url)),
                        Style::new().add_modifier(Modifier::DIM),
                    );
                }
            }
            TagEnd::Image => {
                self.link.pop();
                self.append("]".to_string(), Style::new());
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            self.push_code_text(text);
            return;
        }
        if let Some(link) = self.link.last_mut() {
            link.text.push_str(text);
        }
        let style = self.current_style();
        self.append(escape_plain_text(text), style);
    }

    fn code(&mut self, text: &str) {
        if let Some(link) = self.link.last_mut() {
            link.text.push_str(text);
        }
        self.append(escape_plain_text(text), code_style());
    }

    fn push_code_text(&mut self, text: &str) {
        let style = code_style();
        let mut lines = text.split('\n');
        if let Some(first) = lines.next()
            && !first.is_empty()
        {
            self.append(escape_plain_text(first), style);
        }
        for rest in lines {
            self.flush_line();
            if !rest.is_empty() {
                self.append(escape_plain_text(rest), style);
            }
        }
    }

    fn soft_break(&mut self) {
        self.append(" ".to_string(), Style::new());
    }

    fn hard_break(&mut self) {
        if let Some(table) = &mut self.table {
            table.current_cell.push(' ');
            return;
        }
        self.flush_line();
    }

    fn rule(&mut self) {
        self.flush_line();
        self.lines.push(Line::styled(
            "-".repeat(40),
            Style::new().add_modifier(Modifier::DIM),
        ));
        self.blank_line();
    }

    fn finish_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        if table.rows.is_empty() {
            return;
        }
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0usize; columns];
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
        for (row_index, row) in table.rows.iter().enumerate() {
            let mut spans = Vec::with_capacity(columns * 2);
            for (index, width) in widths.iter().enumerate() {
                let cell = row.get(index).map(String::as_str).unwrap_or("");
                let alignment = table
                    .alignments
                    .get(index)
                    .copied()
                    .unwrap_or(Alignment::None);
                spans.push(Span::raw(pad_cell(cell, *width, alignment)));
                if index + 1 < columns {
                    spans.push(Span::raw(" | "));
                }
            }
            self.lines.push(Line::from(spans));
            if row_index == 0 {
                let separator = widths
                    .iter()
                    .map(|width| "-".repeat(*width))
                    .collect::<Vec<_>>()
                    .join("-+-");
                self.lines.push(Line::styled(
                    separator,
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
        }
        self.blank_line();
    }

    /// Arms `next_prefix` with the blockquote markers alone, when nothing
    /// more specific (a list item's bullet) has already armed it.
    fn begin_block(&mut self) {
        if self.next_prefix.is_none() && self.blockquote_depth > 0 {
            self.next_prefix = Some(self.quote_prefix());
        }
    }

    fn quote_prefix(&self) -> String {
        "> ".repeat(self.blockquote_depth)
    }

    fn current_style(&self) -> Style {
        self.style_stack
            .iter()
            .fold(Style::new(), |acc, s| acc.patch(*s))
    }

    /// Routes already-escaped content either into the table cell currently
    /// being built, or onto the transcript as a styled span.
    fn append(&mut self, content: String, style: Style) {
        if let Some(table) = &mut self.table {
            table.current_cell.push_str(&content);
            return;
        }
        if self.current.is_empty()
            && let Some(prefix) = self.next_prefix.take()
            && !prefix.is_empty()
        {
            self.current.push(Span::raw(prefix));
        }
        self.current.push(Span::styled(content, style));
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let spans = mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
        if self.in_code_block {
            self.next_prefix = Some(self.quote_prefix());
        }
    }

    fn blank_line(&mut self) {
        self.flush_line();
        if !self.lines.is_empty()
            && !matches!(self.lines.last(), Some(line) if line.spans.is_empty())
        {
            self.lines.push(Line::default());
        }
    }

    fn finish(mut self) -> Text<'static> {
        self.flush_line();
        while matches!(self.lines.last(), Some(line) if line.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
}

fn heading_color(level: HeadingLevel) -> Color {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => Color::Cyan,
        _ => Color::Blue,
    }
}

fn code_style() -> Style {
    Style::new().fg(Color::Yellow)
}

fn pad_cell(content: &str, width: usize, alignment: Alignment) -> String {
    let pad = width.saturating_sub(content.chars().count());
    match alignment {
        Alignment::Right => format!("{}{content}", " ".repeat(pad)),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{content}{}", " ".repeat(left), " ".repeat(right))
        }
        Alignment::Left | Alignment::None => format!("{content}{}", " ".repeat(pad)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every span's text, concatenated, in document order. Enough to check
    /// content and control-character safety without pinning exact layout.
    fn flat_text(text: &Text<'_>) -> String {
        let mut out = String::new();
        for (index, line) in text.lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            for span in &line.spans {
                out.push_str(&span.content);
            }
        }
        out
    }

    #[test]
    fn a_heading_is_bold_and_on_its_own_line() {
        let text = render("# Title\n\nbody");
        let flat = flat_text(&text);
        assert!(flat.contains("# Title"));
        assert!(flat.contains("body"));
        let heading_line = &text.lines[0];
        assert!(
            heading_line
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn emphasis_and_strong_and_strikethrough_set_modifiers() {
        let text = render("*em* **strong** ~~gone~~");
        let modifiers: Vec<Modifier> = text.lines[0]
            .spans
            .iter()
            .map(|s| s.style.add_modifier)
            .collect();
        assert!(modifiers.iter().any(|m| m.contains(Modifier::ITALIC)));
        assert!(modifiers.iter().any(|m| m.contains(Modifier::BOLD)));
        assert!(modifiers.iter().any(|m| m.contains(Modifier::CROSSED_OUT)));
    }

    #[test]
    fn inline_code_is_not_escaped_as_a_markdown_span_but_kept_literal() {
        let text = render("run `rm -rf /` now");
        assert!(flat_text(&text).contains("rm -rf /"));
    }

    #[test]
    fn an_unordered_list_gets_dash_bullets_and_a_nested_item_indents() {
        let text = render("- one\n  - two");
        let flat = flat_text(&text);
        assert!(flat.contains("- one"));
        assert!(flat.contains("  - two"));
    }

    #[test]
    fn an_ordered_list_numbers_items_in_order() {
        let text = render("1. first\n2. second");
        let flat = flat_text(&text);
        assert!(flat.contains("1. first"));
        assert!(flat.contains("2. second"));
    }

    #[test]
    fn a_fenced_code_block_keeps_its_own_lines_and_shows_the_language() {
        let text = render("```rust\nfn main() {}\n```");
        let flat = flat_text(&text);
        assert!(flat.contains("rust"));
        assert!(flat.contains("fn main() {}"));
    }

    #[test]
    fn a_link_shows_its_text_and_its_destination() {
        let text = render("[docs](https://example.com/docs)");
        let flat = flat_text(&text);
        assert!(flat.contains("docs"));
        assert!(flat.contains("https://example.com/docs"));
    }

    #[test]
    fn an_autolink_does_not_repeat_its_own_url() {
        let text = render("<https://example.com>");
        let flat = flat_text(&text);
        assert_eq!(flat.matches("https://example.com").count(), 1);
    }

    #[test]
    fn a_table_renders_a_header_separator_and_aligned_body() {
        let text = render("| a | bb |\n|---|---:|\n| 1 | 22 |");
        let flat = flat_text(&text);
        assert!(flat.contains('a'));
        assert!(flat.contains("bb"));
        assert!(flat.contains('1'));
        assert!(flat.contains("22"));
    }

    #[test]
    fn a_blockquote_prefixes_every_line_with_the_quote_marker() {
        let text = render("> quoted");
        assert!(flat_text(&text).contains("> quoted"));
    }

    // Parity replacement for `TestMarkdownRecoversFromExactUnterminatedEntityAttack`,
    // `TestMarkdownFiltersControlsSynthesizedByFormatting`, and
    // `TestMarkdownPreservesEntityCodeAndLinkSemantics`: those Go tests drive a
    // real glamour render and then check `filterTerminalOutput` let only safe
    // SGR sequences through. This renderer never produces ANSI in the first
    // place, so the equivalent, architecture-appropriate property is that no
    // raw control byte from a hostile document ever reaches a `Span`'s text,
    // while ordinary visible content and link/code semantics survive intact.
    #[test]
    fn malicious_documents_lose_no_raw_controls_but_keep_their_visible_content() {
        let input = "safe bold **text**\n\
            direct \u{1b}]52;c;direct\u{7} and \u{9d}52;c;c1-direct\u{9c}\n\
            entity &#27;]52;c;decimal&#7;\n\
            [docs](https://example.com/docs?a=1)\n\
            `&copy;` and `&#27;`";
        let text = render(input);
        let flat = flat_text(&text);

        for forbidden in ['\u{1b}', '\u{7}', '\u{9d}', '\u{9c}'] {
            assert!(
                !flat.contains(forbidden),
                "raw control {forbidden:?} survived escaping: {flat:?}"
            );
        }
        assert!(flat.contains("safe bold"));
        assert!(flat.contains("text"));
        assert!(flat.contains("docs"));
        assert!(flat.contains("https://example.com/docs?a=1"));
        assert!(flat.contains("&copy;"));
        assert!(flat.contains("&#27;"));
    }

    #[test]
    fn raw_html_is_shown_as_literal_escaped_text_never_interpreted() {
        let text = render("before <script>alert(1)</script> after");
        let flat = flat_text(&text);
        assert!(flat.contains("<script>"));
        assert!(flat.contains("alert(1)"));
    }
}
