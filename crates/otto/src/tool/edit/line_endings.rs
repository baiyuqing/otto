//! Line-ending and fuzzy-character normalization with offset maps.

pub(super) fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Converts CRLF and CR to LF and records, for every normalized byte, the
/// offset it came from. The map is `None` when `text` needs no change, meaning
/// offsets are identical.
pub(super) fn normalize_line_endings_with_map(text: &str) -> (String, Option<Vec<usize>>) {
    if !text.contains('\r') {
        return (text.to_owned(), None);
    }
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut offsets = Vec::with_capacity(raw.len() + 1);
    let mut i = 0;
    while i < raw.len() {
        offsets.push(i);
        if raw[i] == b'\r' {
            out.push(b'\n');
            i += if i + 1 < raw.len() && raw[i + 1] == b'\n' {
                2
            } else {
                1
            };
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    offsets.push(raw.len());
    (
        String::from_utf8(out).expect("replacing CR with LF keeps UTF-8 valid"),
        Some(offsets),
    )
}

/// The file's line terminator. A lone CR only counts when the file has no LF at
/// all, so a stray CR inside a line does not change it.
pub(super) fn detect_newline(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else if text.contains('\n') {
        "\n"
    } else if text.contains('\r') {
        "\r"
    } else {
        "\n"
    }
}

pub(super) fn restore_line_endings(text: &str, newline: &str) -> String {
    if newline == "\n" {
        text.to_owned()
    } else {
        text.replace('\n', newline)
    }
}

/// Folds the characters a copy-paste round trip tends to change and drops
/// trailing whitespace per line, recording the source offset of every output
/// byte with one extra entry for the input length.
pub(super) fn normalize_for_fuzzy_match_with_map(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let mut pos = 0;
    while pos < text.len() {
        let line_end = text[pos..].find('\n').map(|index| pos + index);
        let end = line_end.unwrap_or(text.len());
        let trimmed_end = trim_trailing_fuzzy_whitespace(text, pos, end);
        for (index, character) in text[pos..trimmed_end].char_indices() {
            match fuzzy_char(character) {
                Some(replacement) => {
                    for _ in 0..replacement.len() {
                        offsets.push(pos + index);
                    }
                    out.push_str(replacement);
                }
                None => {
                    for _ in 0..character.len_utf8() {
                        offsets.push(pos + index);
                    }
                    out.push(character);
                }
            }
        }
        let Some(line_end) = line_end else {
            break;
        };
        offsets.push(end);
        out.push('\n');
        pos = line_end + 1;
    }
    offsets.push(text.len());
    (out, offsets)
}

/// Returns the end offset with trailing spaces, but not line breaks, removed.
pub(super) fn trim_trailing_fuzzy_whitespace(text: &str, start: usize, mut end: usize) -> usize {
    while end > start {
        let character = text[start..end]
            .chars()
            .next_back()
            .expect("the slice is not empty");
        if !character.is_whitespace() || character == '\n' || character == '\r' {
            break;
        }
        end -= character.len_utf8();
    }
    end
}

/// The canonical form of one character in the fuzzy view, or `None` when the
/// character is already canonical.
fn fuzzy_char(character: char) -> Option<&'static str> {
    Some(match character {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => "'",
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => "\"",
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => "-",
        '\u{00a0}' | '\u{2000}' | '\u{2001}' | '\u{2002}' | '\u{2003}' | '\u{2004}'
        | '\u{2005}' | '\u{2006}' | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200a}'
        | '\u{202f}' | '\u{205f}' | '\u{3000}' => " ",
        _ => return None,
    })
}
