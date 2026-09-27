//! In-place text edits of a `config.toml` document.
//!
//! Every Otto command that changes the configuration file edits its text with
//! these functions instead of serializing a parsed [`super::File`], so each
//! write replaces, inserts, or removes one key statement or one table and
//! leaves every other byte where it was: comments, blank lines, key order,
//! and quoting.
//!
//! Each function parses its own result and compares it with the parsed
//! original plus the intended change. Any other difference, which is what a
//! layout these functions do not handle produces (a dotted key or an inline
//! table in place of a `[table]` header), is returned as an error and the
//! caller writes nothing.

use super::ConfigError;

/// Sets `key` in the table at `table` (the top level when empty) to `value`,
/// a TOML value expression exactly as it should appear in the file, or
/// removes the key when `value` is `None`.
///
/// An existing statement keeps its key spelling, spacing, and trailing
/// comment; only the value is replaced. A missing key is inserted after the
/// table's last statement, or at the start of the table when it has none.
/// `text` is returned unchanged when the key already holds that value.
///
/// Errors: `text` is not a valid document, `value` is not a TOML value, the
/// table is not written as its own `[header]`, or the edit would change
/// anything besides that key.
pub fn set_value(
    text: &str,
    table: &[&str],
    key: &str,
    value: Option<&str>,
) -> Result<String, ConfigError> {
    let mut expected = parse(text)?;
    let parsed = value.map(parse_value).transpose()?;
    let target = table_mut(&mut expected, table).ok_or_else(|| unsupported(table))?;
    if target.get(key) == parsed.as_ref() {
        return Ok(text.to_string());
    }
    match &parsed {
        Some(parsed) => target.insert(key.to_string(), parsed.clone()),
        None => target.remove(key),
    };

    let (start, end) = block(text, table).ok_or_else(|| unsupported(table))?;
    let statements = statements(text, start, end)?;
    let existing = statements.iter().find(|statement| statement.key == key);
    let updated = match (existing, value) {
        (Some(statement), Some(value)) => {
            let (from, to) = value_span(text, statement)?;
            format!("{}{value}{}", &text[..from], &text[to..])
        }
        (Some(statement), None) => {
            format!("{}{}", &text[..statement.start], &text[statement.end..])
        }
        (None, Some(value)) => {
            let at = statements.last().map_or(start, |statement| statement.end);
            insert(text, at, &format!("{key} = {value}\n"))
        }
        (None, None) => text.to_string(),
    };
    verify(&updated, &expected, table)?;
    Ok(updated)
}

/// Appends `body` as the new table at `table`, rendered with its own
/// `[header]` (and headers for any sub-tables it holds) after the last byte of
/// `text`.
///
/// Errors: `text` is not a valid document, `table` is empty or already
/// exists, or the appended table would not parse as that table, as happens
/// when a parent is an inline table.
pub fn insert_table(text: &str, table: &[&str], body: toml::Table) -> Result<String, ConfigError> {
    let (last, parents) = table.split_last().ok_or_else(|| unsupported(table))?;
    let mut expected = parse(text)?;
    let mut target = &mut expected;
    for part in parents {
        target = target
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| unsupported(table))?;
    }
    if target
        .insert(last.to_string(), toml::Value::Table(body.clone()))
        .is_some()
    {
        return Err(unsupported(table));
    }

    let nested = table.iter().rev().fold(body, |inner, part| {
        let mut outer = toml::Table::new();
        outer.insert(part.to_string(), toml::Value::Table(inner));
        outer
    });
    let rendered = toml::to_string(&nested).map_err(|error| ConfigError::new(error.to_string()))?;
    let updated = insert(text, text.len(), &rendered);
    verify(&updated, &expected, table)?;
    Ok(updated)
}

/// Removes the table at `table` and every sub-table written under it, from
/// its first `[header]` through the last statement of its last one.
///
/// Comments and blank lines after that last statement stay, since they
/// usually introduce the table that follows; so do comments above the first
/// header.
///
/// Errors: `text` is not a valid document, the table does not exist, or it is
/// not written as its own `[header]`.
pub fn remove_table(text: &str, table: &[&str]) -> Result<String, ConfigError> {
    let (last, parents) = table.split_last().ok_or_else(|| unsupported(table))?;
    let mut expected = parse(text)?;
    table_mut(&mut expected, parents)
        .and_then(|parent| parent.remove(*last))
        .ok_or_else(|| unsupported(table))?;
    // A parent that exists only because a removed header named it, like
    // `mcp.servers` for a lone `[mcp.servers.docs]`, is gone from the result.
    for depth in (1..table.len()).rev() {
        let implicit = block(text, &table[..depth]).is_none()
            && table_mut(&mut expected, &table[..depth]).is_some_and(|parent| parent.is_empty());
        if !implicit {
            break;
        }
        if let Some(grandparent) = table_mut(&mut expected, &table[..depth - 1]) {
            grandparent.remove(table[depth - 1]);
        }
    }

    let headers = table_headers(text);
    let removed: Vec<bool> = headers
        .iter()
        .map(|header| header.path().is_some_and(|path| starts_with(&path, table)))
        .collect();
    let mut updated = text.to_string();
    for (index, header) in headers.iter().enumerate().rev() {
        if !removed[index] {
            continue;
        }
        let body = header.line + header.text.len();
        let next = headers.get(index + 1).map_or(text.len(), |next| next.line);
        let end = if removed.get(index + 1) == Some(&true) {
            next
        } else {
            statements(text, body, next)?
                .last()
                .map_or(body, |statement| statement.end)
        };
        updated.replace_range(header.line..end, "");
    }
    verify(&updated, &expected, table)?;
    Ok(updated)
}

fn parse(text: &str) -> Result<toml::Table, ConfigError> {
    toml::from_str(text).map_err(|_| invalid())
}

fn parse_value(value: &str) -> Result<toml::Value, ConfigError> {
    let mut table = parse(&format!("value = {value}"))?;
    table.remove("value").ok_or_else(invalid)
}

fn table_mut<'a>(root: &'a mut toml::Table, table: &[&str]) -> Option<&'a mut toml::Table> {
    table
        .iter()
        .try_fold(root, |current, part| current.get_mut(*part)?.as_table_mut())
}

fn verify(updated: &str, expected: &toml::Table, table: &[&str]) -> Result<(), ConfigError> {
    match toml::from_str::<toml::Table>(updated) {
        Ok(after) if after == *expected => Ok(()),
        _ => Err(unsupported(table)),
    }
}

/// `text` with `inserted` placed at byte `at`, preceded by a line break when
/// the text before `at` does not end with one.
fn insert(text: &str, at: usize, inserted: &str) -> String {
    let separator = if at > 0 && !text[..at].ends_with('\n') {
        "\n"
    } else {
        ""
    };
    format!("{}{separator}{inserted}{}", &text[..at], &text[at..])
}

fn invalid() -> ConfigError {
    ConfigError::new("invalid configuration")
}

fn unsupported(table: &[&str]) -> ConfigError {
    let name = if table.is_empty() {
        "the top level".to_string()
    } else {
        format!("[{}]", table.join("."))
    };
    ConfigError::new(format!(
        "cannot edit {name} in place: it must be written as its own table header, without dotted keys or inline tables; the configuration was not changed"
    ))
}

fn starts_with(path: &[String], prefix: &[&str]) -> bool {
    path.len() >= prefix.len() && path.iter().zip(prefix).all(|(part, want)| part == want)
}

/// The byte range of the body of `table`: after its header line up to the
/// next header, or, for the top level, everything before the first header.
fn block(text: &str, table: &[&str]) -> Option<(usize, usize)> {
    let headers = table_headers(text);
    if table.is_empty() {
        return Some((0, headers.first().map_or(text.len(), |first| first.line)));
    }
    let index = headers.iter().position(|header| {
        header
            .path()
            .is_some_and(|path| path.len() == table.len() && starts_with(&path, table))
    })?;
    let header = &headers[index];
    let end = headers.get(index + 1).map_or(text.len(), |next| next.line);
    Some((header.line + header.text.len(), end))
}

/// One key/value statement, from the first byte of its line through the line
/// break after its value, which may span several lines.
struct Statement {
    start: usize,
    end: usize,
    key: String,
}

/// Every statement in `text[start..end]`, which must begin at a statement
/// boundary. Blank and comment lines between statements are skipped; a
/// statement ends at the first line break after which it parses on its own.
fn statements(text: &str, start: usize, end: usize) -> Result<Vec<Statement>, ConfigError> {
    let mut found = Vec::new();
    let mut position = start;
    while position < end {
        let line_end = next_line(text, position, end);
        let line = text[position..line_end].trim();
        if line.is_empty() || line.starts_with('#') {
            position = line_end;
            continue;
        }
        let mut stop = line_end;
        let key = loop {
            if let Ok(parsed) = toml::from_str::<toml::Table>(&text[position..stop])
                && let Some(key) = parsed.keys().next()
            {
                break key.clone();
            }
            if stop == end {
                return Err(invalid());
            }
            stop = next_line(text, stop, end);
        };
        found.push(Statement {
            start: position,
            end: stop,
            key,
        });
        position = stop;
    }
    Ok(found)
}

fn next_line(text: &str, position: usize, end: usize) -> usize {
    text[position..end]
        .find('\n')
        .map_or(end, |index| position + index + 1)
}

/// The byte range of `statement`'s value, without the whitespace around it or
/// a trailing comment.
///
/// The value ends at the first `#` (or the statement end) before which the
/// text parses as a value, so a `#` inside a string is not taken for a
/// comment.
fn value_span(text: &str, statement: &Statement) -> Result<(usize, usize), ConfigError> {
    let body = &text[statement.start..statement.end];
    let equals = body.find('=').ok_or_else(invalid)?;
    let after = &body[equals + 1..];
    let from = statement.start + equals + 1 + (after.len() - after.trim_start().len());
    let rest = &text[from..statement.end];
    let cuts = rest
        .match_indices('#')
        .map(|(index, _)| index)
        .chain([rest.len()]);
    for cut in cuts {
        let value = rest[..cut].trim_end();
        if parse_value(value).is_ok() {
            return Ok((from, from + value.len()));
        }
    }
    Err(invalid())
}

/// One table header line, located by byte offset.
struct Header<'a> {
    /// Offset of the first byte of the line the header starts on.
    line: usize,
    text: &'a str,
}

impl Header<'_> {
    /// The decoded key path of a `[a.b]` header; `None` for an
    /// array-of-tables `[[a.b]]` header.
    fn path(&self) -> Option<Vec<String>> {
        let mut table = toml::from_str::<toml::Table>(self.text).ok()?;
        let mut path = Vec::new();
        while !table.is_empty() {
            if table.len() != 1 {
                return None;
            }
            let (key, value) = table.into_iter().next()?;
            let toml::Value::Table(inner) = value else {
                return None;
            };
            path.push(key);
            table = inner;
        }
        (!path.is_empty()).then_some(path)
    }
}

/// Every table header in `text`, in document order.
///
/// A line that merely looks like a header may be text inside a multi-line
/// string or an array element, so a candidate counts only when the document
/// truncated just before it still parses: a truncation in the middle of any
/// multi-line construct does not. `text` must already be a valid document.
fn table_headers(text: &str) -> Vec<Header<'_>> {
    let mut headers = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if line.trim_start().starts_with('[')
            && toml::from_str::<toml::Table>(&text[..start]).is_ok()
        {
            headers.push(Header {
                line: start,
                text: line,
            });
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_value_replaces_only_the_value() {
        let text = "# top\n[a] # header\nx = 'old' # note\ny = \"#not a comment\" # real\n\n# next\n[b]\nx = 1\n";
        assert_eq!(
            set_value(text, &["a"], "x", Some("\"new\"")).expect("set"),
            text.replace("'old'", "\"new\"")
        );
        assert_eq!(
            set_value(text, &["a"], "y", Some("2")).expect("set"),
            text.replace("\"#not a comment\"", "2")
        );
    }

    #[test]
    fn set_value_replaces_a_multi_line_value() {
        let text = "[a]\nx = [\n  1, # one\n  2,\n] # list\ny = 3\n";
        assert_eq!(
            set_value(text, &["a"], "x", Some("[4]")).expect("set"),
            "[a]\nx = [4] # list\ny = 3\n"
        );
    }

    #[test]
    fn set_value_inserts_after_the_last_statement_and_removes() {
        let text = "[a]\nx = 1\n\n# about b\n[b]\n";
        let inserted = set_value(text, &["a"], "y", Some("2")).expect("insert");
        assert_eq!(inserted, "[a]\nx = 1\ny = 2\n\n# about b\n[b]\n");
        assert_eq!(
            set_value(&inserted, &["a"], "y", None).expect("remove"),
            text
        );
        assert_eq!(
            set_value(text, &["b"], "z", Some("true")).expect("insert"),
            "[a]\nx = 1\n\n# about b\n[b]\nz = true\n"
        );
        assert_eq!(
            set_value("[a]\nx = 1", &["a"], "y", Some("2")).expect("no final newline"),
            "[a]\nx = 1\ny = 2\n"
        );
    }

    #[test]
    fn set_value_leaves_an_unchanged_value_untouched() {
        let text = "[a]\nx = [ 1,\n 2 ] # keep\n";
        assert_eq!(
            set_value(text, &["a"], "x", Some("[1, 2]")).expect("same"),
            text
        );
        assert_eq!(
            set_value(text, &["a"], "missing", None).expect("absent"),
            text
        );
    }

    #[test]
    fn set_value_matches_quoted_header_keys_and_ignores_headers_in_strings() {
        let text = "[p]\ns = '''\n[q.\"a.b\"]\nx = 0\n'''\n[q.\"a.b\"]\nx = 1\n";
        assert_eq!(
            set_value(text, &["q", "a.b"], "x", Some("2")).expect("set"),
            text.replace("x = 1", "x = 2")
        );
    }

    #[test]
    fn set_value_refuses_layouts_it_cannot_edit_in_place() {
        for text in [
            "a.x = 1\n",
            "a = { x = 1 }\n",
            "[a]\nx.y = 1\n",
            "[a.b]\ny = 1\n",
        ] {
            let error = set_value(text, &["a"], "x", Some("2")).expect_err(text);
            assert!(
                error.to_string().contains("configuration was not changed"),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn insert_and_remove_table_round_trip_the_original_bytes() {
        let text = "# top\n[a]\nx = 1\n\n# about b\n[b]\ny = 2\n";
        let mut body = toml::Table::new();
        body.insert("k".into(), "v".into());
        let mut env = toml::Table::new();
        env.insert("E".into(), "F".into());
        body.insert("env".into(), toml::Value::Table(env));

        let inserted = insert_table(text, &["a", "new one"], body).expect("insert");
        assert!(inserted.starts_with(text), "{inserted}");
        let parsed = parse(&inserted).expect("parse");
        assert_eq!(parsed["a"]["new one"]["env"]["E"].as_str(), Some("F"));

        assert_eq!(
            remove_table(&inserted, &["a", "new one"]).expect("remove"),
            text
        );
        assert_eq!(
            remove_table(text, &["a"]).expect("remove a"),
            "# top\n\n# about b\n[b]\ny = 2\n"
        );
        assert_eq!(
            remove_table("# servers\n[m.s.one]\nx = 1\n# end\n", &["m", "s", "one"])
                .expect("remove the only child"),
            "# servers\n# end\n"
        );
    }

    #[test]
    fn insert_table_refuses_an_inline_parent_and_an_existing_table() {
        assert!(insert_table("a = {}\n", &["a", "b"], toml::Table::new()).is_err());
        assert!(insert_table("[a.b]\n", &["a", "b"], toml::Table::new()).is_err());
    }

    #[test]
    fn remove_table_refuses_an_inline_or_missing_table() {
        assert!(remove_table("a = { b = { x = 1 } }\n", &["a", "b"]).is_err());
        assert!(remove_table("[a]\n", &["a", "b"]).is_err());
    }
}
