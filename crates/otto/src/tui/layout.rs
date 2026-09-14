//! Small pure-logic helpers shared by the footer and the transcript: size
//! thresholds, token/percentage formatting, and the control-character
//! escaping every piece of untrusted text goes through before it reaches a
//! `ratatui::text::Span`.
//!
//! Port of the non-layout parts of `internal/tui/layout.go`. Ratatui owns
//! wrapping and clipping, so `render.rs` puts essential footer fields first
//! and lets the widget clip the rest.

/// The smallest terminal ratatui's TUI will actually lay out. Below this,
/// [`crate::tui::run`] shows the "terminal is too small" message instead of
/// the normal view. Port of `minTerminalWidth`/`minTerminalHeight`.
pub const MIN_TERMINAL_WIDTH: u16 = 40;
pub const MIN_TERMINAL_HEIGHT: u16 = 8;

/// Below this height the composer collapses to a single input line instead
/// of a boxed multi-line editor. Port of `inputBoxThreshold`.
pub const INPUT_BOX_THRESHOLD: u16 = 12;

/// The status bar reports the fraction of the context window in use as one
/// decimal place, rounded to the nearest tenth of a percent. Port of
/// `formatFooterContextPercentage` (the `big.Int` there guards against
/// overflow on 32-bit Go; token counts fit `i64` comfortably here).
pub fn format_context_percentage(input_tokens: i64, context_window: i64) -> String {
    let input_tokens = input_tokens.max(0);
    if context_window <= 0 {
        return "0.0%".to_string();
    }
    let numerator = input_tokens.saturating_mul(1000) + context_window / 2;
    let tenths = (numerator / context_window).max(0);
    let digits = tenths.to_string();
    if digits.len() == 1 {
        format!("0.{digits}%")
    } else {
        let (whole, tenth) = digits.split_at(digits.len() - 1);
        format!("{whole}.{tenth}%")
    }
}

/// Formats a token count as `k`/`M`/`B`, rounded to the nearest tenth of the
/// unit. Port of `formatFooterTokenCount`/`formatFooterTokenCountUnit`.
///
/// ponytail: Go has a second, k-only formatter in `compaction.go`
/// (`formatCompactionTokenCount`) that truncates instead of rounding, so it
/// disagrees with this one on values like 12360 (it prints "12.3k"; this
/// prints "12.4k"). Both formatters are cosmetic (a status line, not a
/// money or security path), so `entries.rs`'s compaction line and the footer
/// share this single rounding formatter instead of keeping two. Upgrade
/// path: reintroduce the truncating variant if a test pins the old text.
pub fn format_token_count(tokens: i64) -> String {
    if tokens <= 0 {
        return "0".to_string();
    }
    let count = tokens as u64;
    if count < 1_000 {
        return count.to_string();
    }
    if count < 1_000_000 {
        return format_token_count_unit(count, 1_000, "k", "M", false);
    }
    if count < 1_000_000_000 {
        return format_token_count_unit(count, 1_000_000, "M", "B", false);
    }
    format_token_count_unit(count, 1_000_000_000, "B", "", true)
}

fn format_token_count_unit(
    count: u64,
    divisor: u64,
    suffix: &str,
    next_suffix: &str,
    largest: bool,
) -> String {
    let whole = count / divisor;
    let rem = count % divisor;
    let tenths = (rem * 10 + divisor / 2) / divisor;
    let whole = whole + tenths / 10;
    let tenths = tenths % 10;
    if !largest && whole >= 1000 {
        return format!("1{next_suffix}");
    }
    if tenths == 0 {
        format!("{whole}{suffix}")
    } else {
        format!("{whole}.{tenths}{suffix}")
    }
}

/// The footer's workspace field is the directory's last path component, not
/// its full path. Port of `footerWorkspace`; Otto is macOS-only, so this
/// works in `/`-separated paths rather than `std::path`'s platform-generic
/// (and here, unneeded) separator handling.
pub fn footer_workspace(workspace: &str) -> String {
    if workspace.is_empty() {
        return String::new();
    }
    let trimmed = workspace.trim_end_matches('/');
    let base = if trimmed.is_empty() {
        "/"
    } else {
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    };
    if base.is_empty() || base == "." || base == "/" {
        workspace.to_string()
    } else {
        base.to_string()
    }
}

/// Escapes control characters other than `\n`/`\t` for text that may span
/// multiple lines (transcript entries, tool output). Port of
/// `escapePlainText`.
pub fn escape_plain_text(text: &str) -> String {
    escape_text_controls(text, true)
}

/// Escapes every control character, including `\n`/`\t`, for text that must
/// render on one line (footer fields, status text). Port of
/// `escapeSingleLineText`.
pub fn escape_single_line_text(text: &str) -> String {
    escape_text_controls(text, false)
}

/// Security: a `ratatui`/`crossterm` `Span` prints its content straight to
/// the terminal, so a raw ESC (or other control byte) inside model or tool
/// output would be interpreted by the real terminal, not by ratatui. Port of
/// `escapeTextControls`.
fn escape_text_controls(text: &str, preserve_multiline_whitespace: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if preserve_multiline_whitespace && (ch == '\n' || ch == '\t') {
            out.push(ch);
        } else if ch.is_control() {
            let code = ch as u32;
            if code < 0x100 {
                out.push_str(&format!("\\x{code:02x}"));
            } else {
                out.push_str(&format!("\\u{code:04x}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_percentage_rounds_to_the_nearest_tenth() {
        assert_eq!(format_context_percentage(0, 1000), "0.0%");
        assert_eq!(format_context_percentage(500, 1000), "50.0%");
        assert_eq!(format_context_percentage(1, 3), "33.3%");
        assert_eq!(format_context_percentage(2, 3), "66.7%");
    }

    #[test]
    fn context_percentage_treats_a_missing_window_as_zero() {
        assert_eq!(format_context_percentage(500, 0), "0.0%");
        assert_eq!(format_context_percentage(-5, 1000), "0.0%");
    }

    #[test]
    fn token_counts_round_to_the_nearest_tenth_of_the_unit() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1k");
        assert_eq!(format_token_count(12360), "12.4k");
        assert_eq!(format_token_count(999_950), "1M");
        assert_eq!(format_token_count(1_500_000), "1.5M");
        assert_eq!(format_token_count(2_500_000_000), "2.5B");
    }

    #[test]
    fn the_footer_workspace_is_the_last_path_component() {
        assert_eq!(footer_workspace(""), "");
        assert_eq!(footer_workspace("/"), "/");
        assert_eq!(footer_workspace("/Users/otto/work"), "work");
        assert_eq!(footer_workspace("/Users/otto/work/"), "work");
    }

    #[test]
    fn plain_text_keeps_newlines_and_tabs_but_escapes_other_controls() {
        assert_eq!(escape_plain_text("a\nb\tc\u{7}d"), "a\nb\tc\\x07d");
    }

    #[test]
    fn single_line_text_escapes_newlines_too() {
        assert_eq!(escape_single_line_text("a\nb"), "a\\x0ab");
    }
}
