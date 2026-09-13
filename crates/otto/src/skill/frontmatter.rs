//! The `SKILL.md` frontmatter parser. Port of `internal/skill/frontmatter.go`.
//!
//! The grammar is a deliberate YAML subset: `---` delimiters, `key: value`
//! lines with plain, single-quoted, double-quoted, literal (`|`) and folded
//! (`>`) scalars, nested blocks (skipped, not flattened), blank lines and `#`
//! comments. Anything outside the subset is an error rather than a guess, so a
//! malformed skill is dropped with a warning instead of being half-read.

use std::collections::BTreeMap;

/// The frontmatter fields of one `SKILL.md`, keyed by their top-level key.
///
/// Go returns a `map[string]string`; a [`BTreeMap`] matches it and additionally
/// fixes iteration order, which the callers rely on for stable warnings.
pub type Fields = BTreeMap<String, String>;

/// Splits `data` into its frontmatter fields and the Markdown body.
///
/// Errors carry Go's exact text, because they surface to the user as
/// discovery warnings.
pub fn parse(data: &[u8]) -> Result<(Fields, String), String> {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.is_empty() || !is_delimiter(lines[0]) {
        return Err("missing frontmatter".to_string());
    }
    let end = lines[1..]
        .iter()
        .position(|line| is_delimiter(line))
        .map(|offset| offset + 1)
        .ok_or_else(|| "unterminated frontmatter".to_string())?;

    let fields = parse_fields(&lines[1..end])?;
    let body = lines[end + 1..].join("\n");
    let body = body.strip_prefix('\n').unwrap_or(&body).to_string();
    Ok((fields, body))
}

fn is_delimiter(line: &str) -> bool {
    line.strip_suffix('\r').unwrap_or(line) == "---"
}

/// Whether a key is Go's `^[A-Za-z0-9_-]+$`.
fn is_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Whether a value indicator is Go's `^[|>][-+]?$`. The chomping indicator is
/// accepted and ignored, as in Go.
fn is_block_scalar(rest: &str) -> bool {
    let mut bytes = rest.bytes();
    match bytes.next() {
        Some(b'|' | b'>') => {}
        _ => return false,
    }
    match bytes.next() {
        None => true,
        Some(b'-' | b'+') => bytes.next().is_none(),
        Some(_) => false,
    }
}

/// Parses the lines strictly between the delimiters. `lines[i]` is file line
/// `i + 2`, 1-based, which is what the error messages report.
fn parse_fields(lines: &[&str]) -> Result<Fields, String> {
    let mut fields = Fields::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        let line_number = index + 2;
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(format!("unsupported frontmatter line {line_number}"));
        }
        let (key, rest) =
            split_key(line).ok_or_else(|| format!("unsupported frontmatter line {line_number}"))?;
        let (value, consumed) = parse_value(rest, lines, index + 1, line_number)?;
        fields.insert(key.to_string(), value);
        index += 1 + consumed;
    }
    Ok(fields)
}

/// Splits a `key: value` line into its key and the remainder after the
/// separator, or `None` when the line does not match the key grammar.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let separator = line.find(':')?;
    let key = &line[..separator];
    if !is_key(key) {
        return None;
    }
    let after = &line[separator + 1..];
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    Some((key, after.strip_prefix(' ').unwrap_or(after)))
}

/// Parses the value that follows `key:`, consuming the continuation lines it
/// owns. `lines[start..]` are the lines after the key line; `line_number` is
/// the key line's 1-based file line number, used only for error messages.
fn parse_value(
    rest: &str,
    lines: &[&str],
    start: usize,
    line_number: usize,
) -> Result<(String, usize), String> {
    if rest.is_empty() {
        // A nested block is skipped rather than flattened into the top level.
        let (_, consumed) = collect_indented_block(lines, start);
        return Ok((String::new(), consumed));
    }
    if is_block_scalar(rest) {
        let (raw, consumed) = collect_indented_block(lines, start);
        let content = if rest.starts_with('|') {
            literal_block_content(&raw)
        } else {
            folded_block_content(&raw)
        };
        return Ok((content, consumed));
    }
    match rest.as_bytes()[0] {
        b'"' => parse_double_quoted(rest)
            .map(|value| (value, 0))
            .map_err(|error| format!("frontmatter line {line_number}: {error}")),
        b'\'' => parse_single_quoted(rest)
            .map(|value| (value, 0))
            .map_err(|error| format!("frontmatter line {line_number}: {error}")),
        _ => {
            let (continuation, consumed) = collect_plain_continuation(lines, start);
            let mut parts = Vec::with_capacity(continuation.len() + 1);
            parts.push(rest.trim().to_string());
            parts.extend(continuation);
            Ok((parts.join(" "), consumed))
        }
    }
}

/// The raw lines from `lines[start]` that belong to a nested block or block
/// scalar: blank lines and lines indented with a leading space or tab. It
/// stops at the first non-blank line at column 0, or at the end of input.
fn collect_indented_block(lines: &[&str], start: usize) -> (Vec<String>, usize) {
    let mut collected = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() {
            collected.push(String::new());
        } else if line.starts_with([' ', '\t']) {
            collected.push(line.to_string());
        } else {
            break;
        }
        index += 1;
    }
    (collected, index - start)
}

/// The continuation lines of a multi-line plain scalar: only non-blank lines
/// indented with a leading space or tab. A blank line ends the scalar without
/// being consumed.
fn collect_plain_continuation(lines: &[&str], start: usize) -> (Vec<String>, usize) {
    let mut collected = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() || !line.starts_with([' ', '\t']) {
            break;
        }
        collected.push(line.trim().to_string());
        index += 1;
    }
    (collected, index - start)
}

/// Removes the leading whitespace of the first non-blank line from every line,
/// per the YAML rule that a block scalar's indentation is set by its first
/// content line.
fn strip_block_indent(lines: &[String]) -> Vec<String> {
    let Some(indent) = lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches([' ', '\t']).len())
    else {
        return lines.to_vec();
    };
    lines
        .iter()
        .map(|line| {
            if line.len() >= indent {
                line[indent..].to_string()
            } else {
                line.trim_start_matches([' ', '\t']).to_string()
            }
        })
        .collect()
}

fn trim_trailing_blank(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

fn literal_block_content(raw: &[String]) -> String {
    let lines = trim_trailing_blank(strip_block_indent(raw));
    if lines.is_empty() {
        return String::new();
    }
    lines.join("\n") + "\n"
}

fn folded_block_content(raw: &[String]) -> String {
    let lines = trim_trailing_blank(strip_block_indent(raw));
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut previous_blank = false;
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            out.push('\n');
            previous_blank = true;
            continue;
        }
        if index > 0 && !previous_blank {
            out.push(' ');
        }
        out.push_str(line);
        previous_blank = false;
    }
    out.push('\n');
    out
}

fn parse_double_quoted(value: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut characters = value.chars().skip(1);
    while let Some(character) = characters.next() {
        match character {
            '"' => return Ok(out),
            '\\' => match characters.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(escaped) => out.push(escaped),
                // Go's loop only consumes an escape when one byte follows, so
                // a trailing backslash stays literal.
                None => out.push('\\'),
            },
            _ => out.push(character),
        }
    }
    Err("unterminated double-quoted value".to_string())
}

fn parse_single_quoted(value: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut characters = value.chars().skip(1).peekable();
    while let Some(character) = characters.next() {
        if character != '\'' {
            out.push(character);
            continue;
        }
        if characters.peek() == Some(&'\'') {
            characters.next();
            out.push('\'');
            continue;
        }
        return Ok(out);
    }
    Err("unterminated single-quoted value".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(data: &str) -> Fields {
        parse(data.as_bytes()).expect("the frontmatter parses").0
    }

    /// Go's `TestParseFrontmatterPlainScalar`.
    #[test]
    fn a_plain_scalar_keeps_its_text_and_the_body_follows() {
        let (fields, body) =
            parse(b"---\nname: pdf\ndescription: Extract text from PDFs\n---\n# PDF\n")
                .expect("the frontmatter parses");
        assert_eq!(fields["name"], "pdf");
        assert_eq!(fields["description"], "Extract text from PDFs");
        assert_eq!(body, "# PDF\n");
    }

    /// Go's `TestParseFrontmatterPlainScalarMultiLine`.
    #[test]
    fn a_plain_scalar_folds_its_indented_continuation_lines() {
        let parsed = fields(
            "---\ndescription: Extract text\n  from PDFs\n  and merge them\nname: pdf\n---\nbody\n",
        );
        assert_eq!(
            parsed["description"],
            "Extract text from PDFs and merge them"
        );
        assert_eq!(parsed["name"], "pdf");
    }

    /// Go's `TestParseFrontmatterDoubleQuotedWithEscapes` and
    /// `TestParseFrontmatterDoubleQuotedUnknownEscape`.
    #[test]
    fn a_double_quoted_scalar_resolves_its_escapes() {
        let parsed =
            fields("---\ndescription: \"Say \\\"hi\\\"\\nnext line\\tend\\\\done\"\n---\nbody\n");
        assert_eq!(parsed["description"], "Say \"hi\"\nnext line\tend\\done");
        assert_eq!(fields("---\nname: \"a\\qb\"\n---\nbody\n")["name"], "aqb");
    }

    /// Go's `TestParseFrontmatterSingleQuotedEscape`.
    #[test]
    fn a_single_quoted_scalar_unescapes_a_doubled_quote() {
        assert_eq!(
            fields("---\ndescription: 'it''s a test'\n---\nbody\n")["description"],
            "it's a test"
        );
    }

    /// Go's `TestParseFrontmatterLiteralBlock` and
    /// `TestParseFrontmatterLiteralBlockWithChompingIndicator`.
    #[test]
    fn a_literal_block_keeps_its_line_breaks_with_or_without_a_chomping_indicator() {
        for indicator in ["|", "|-"] {
            let parsed = fields(&format!(
                "---\ndescription: {indicator}\n  line one\n  line two\n---\nbody\n"
            ));
            assert_eq!(parsed["description"], "line one\nline two\n", "{indicator}");
        }
    }

    /// Go's `TestParseFrontmatterFoldedBlockWithBlankLine`.
    #[test]
    fn a_folded_block_joins_lines_and_keeps_paragraph_breaks() {
        let parsed = fields(
            "---\ndescription: >\n  This is line one\n  continued.\n\n  Second paragraph.\n---\nbody\n",
        );
        assert_eq!(
            parsed["description"],
            "This is line one continued.\nSecond paragraph.\n"
        );
    }

    /// Go's `TestParseFrontmatterNestedBlockSkipped`: a nested key must not
    /// reach the top-level fields, where it could impersonate `name`.
    #[test]
    fn a_nested_block_is_skipped_rather_than_flattened() {
        let parsed = fields(
            "---\nname: pdf\nmetadata:\n  category: files\n  version: \"1.0\"\ndescription: d\n---\nbody\n",
        );
        assert_eq!(
            (parsed["name"].as_str(), parsed["description"].as_str()),
            ("pdf", "d")
        );
        assert!(!parsed.contains_key("category"), "{parsed:?}");
        assert_eq!(parsed["metadata"], "");
    }

    /// Go's `TestParseFrontmatterCommentsIgnored`. The indented comment sits
    /// after a double-quoted value, which never consumes continuation lines,
    /// so it is a standalone comment rather than part of the scalar.
    #[test]
    fn comments_are_ignored_at_any_indentation() {
        let parsed = fields(
            "---\n# a comment\nname: \"pdf\"\n  # indented comment between blocks\ndescription: d\n---\nbody\n",
        );
        assert_eq!(
            (parsed["name"].as_str(), parsed["description"].as_str()),
            ("pdf", "d")
        );
    }

    /// Go's `TestParseFrontmatterDuplicateKeyLastWins`.
    #[test]
    fn a_duplicate_key_takes_its_last_value() {
        assert_eq!(
            fields("---\nname: first\nname: second\n---\nbody\n")["name"],
            "second"
        );
    }

    /// Go's `TestParseFrontmatterMissingFrontmatter`,
    /// `TestParseFrontmatterUnterminated`,
    /// `TestParseFrontmatterUnsupportedLine`,
    /// `TestParseFrontmatterUnterminatedDoubleQuoted` and
    /// `TestParseFrontmatterUnterminatedSingleQuoted`.
    #[test]
    fn malformed_frontmatter_is_reported_rather_than_guessed_at() {
        for (data, want) in [
            ("# just markdown\n", "missing frontmatter"),
            ("---\nname: pdf\n", "unterminated frontmatter"),
            (
                "---\nnot a key line\n---\nbody\n",
                "unsupported frontmatter line 2",
            ),
            (
                "---\nname: \"unterminated\n---\nbody\n",
                "frontmatter line 2: unterminated double-quoted value",
            ),
            (
                "---\nname: 'unterminated\n---\nbody\n",
                "frontmatter line 2: unterminated single-quoted value",
            ),
        ] {
            assert_eq!(parse(data.as_bytes()).expect_err(data), want);
        }
    }

    /// Go's `TestParseFrontmatterDelimiterToleratesCR`.
    #[test]
    fn a_carriage_return_before_the_newline_is_tolerated() {
        assert_eq!(fields("---\r\nname: pdf\r\n---\r\nbody\r\n")["name"], "pdf");
    }

    /// Go's `TestParseFrontmatterEmptyValue`.
    #[test]
    fn a_key_with_no_value_reads_as_empty() {
        assert_eq!(
            fields("---\nname:\ndescription: d\n---\nbody\n")["name"],
            ""
        );
    }

    /// Go's `TestParseFrontmatterBodyLeadingNewlineRemovedOnce`.
    #[test]
    fn only_one_leading_newline_is_stripped_from_the_body() {
        let (_, body) =
            parse(b"---\nname: pdf\n---\n\n\n# heading\n").expect("the frontmatter parses");
        assert_eq!(body, "\n# heading\n");
    }
}
