//! Rewriting the `[sandbox]` table in an existing `config.toml`.
//!
//! `kite sandbox setup` is the only writer of a user's configuration file, so
//! it replaces exactly the `[sandbox]` block and leaves every other byte,
//! including comments and formatting, where it was. Unusual layouts are
//! rejected rather than rewritten destructively.
//!
//! The emitted block is byte-identical to what go-toml v2 marshals for the same
//! [`SandboxConfig`], which is the encoding existing `config.toml` files were
//! written with.

use super::{ConfigError, SandboxConfig, resolve_sandbox};

/// Replaces only the `[sandbox]` table, preserving other TOML bytes.
///
/// Errors: the content must already be a valid document, the existing
/// `[sandbox]` table must be a table header rather than dotted keys, and the
/// round trip must leave every other table untouched.
pub fn update_sandbox(content: &[u8], settings: &SandboxConfig) -> Result<Vec<u8>, ConfigError> {
    resolve_sandbox(settings, None)?;
    let invalid = || ConfigError::new("invalid configuration");
    let text = std::str::from_utf8(content).map_err(|_| invalid())?;
    let mut before: toml::Table = toml::from_str(text).map_err(|_| invalid())?;
    let block = marshal_sandbox(settings);

    let (mut start, mut end) = (None, text.len());
    for header in table_headers(text) {
        if start.is_some() {
            end = header.line;
            break;
        }
        if header.single_table_named("sandbox") {
            start = Some(header.line);
        }
    }
    let updated = match start {
        None => format!("{text}\n{block}"),
        Some(start) => format!("{}{block}{}", &text[..start], &text[end..]),
    };

    let layout = || {
        ConfigError::new("setup requires a separate [sandbox] table; configuration was not changed")
    };
    let mut after: toml::Table = toml::from_str(&updated).map_err(|_| layout())?;
    let want: toml::Table = toml::from_str(&block).map_err(|_| layout())?;
    if after.get("sandbox") != want.get("sandbox") {
        return Err(ConfigError::new("unsupported sandbox layout"));
    }
    before.remove("sandbox");
    after.remove("sandbox");
    if before != after {
        return Err(ConfigError::new(
            "setup would change unrelated configuration",
        ));
    }
    Ok(updated.into_bytes())
}

/// One table header expression, located by byte offset.
struct Header<'a> {
    /// Offset of the first byte of the line the header starts on.
    line: usize,
    text: &'a str,
}

impl Header<'_> {
    /// Whether this is `[name]`: one key part, not `[[name]]` and not
    /// `[name.sub]`. Parsing the header line on its own gives the same answer
    /// as reading the parsed key nodes, with quoted keys already decoded.
    fn single_table_named(&self, name: &str) -> bool {
        let Ok(table) = toml::from_str::<toml::Table>(self.text) else {
            return false;
        };
        let mut entries = table.into_iter();
        let Some((key, value)) = entries.next() else {
            return false;
        };
        entries.next().is_none()
            && key == name
            && value.as_table().is_some_and(toml::Table::is_empty)
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

/// The `[sandbox]` block exactly as go-toml v2 marshals it: a header line,
/// then the four fields in declaration order, with absent optionals omitted
/// and absent lists written as `[]`.
fn marshal_sandbox(settings: &SandboxConfig) -> String {
    let mut block = String::from("[sandbox]\n");
    if let Some(driver) = &settings.driver {
        block.push_str(&format!("driver = {}\n", encode_string(driver)));
    }
    if let Some(network) = &settings.network {
        block.push_str(&format!("network = {}\n", encode_string(network)));
    }
    block.push_str(&format!(
        "read_paths = {}\n",
        encode_array(&settings.read_paths)
    ));
    block.push_str(&format!(
        "allow_env = {}\n",
        encode_array(&settings.allow_env)
    ));
    block
}

fn encode_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|value| encode_string(value)).collect();
    format!("[{}]", items.join(", "))
}

/// Follows go-toml's `encodeString`: a literal string unless the value contains
/// a quote, a line break or a control character.
fn encode_string(value: &str) -> String {
    if !needs_quoting(value) {
        return format!("'{value}'");
    }
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'"');
    for byte in value.as_bytes() {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            byte if invalid_ascii(*byte) => {
                out.extend_from_slice(format!("\\u00{byte:02X}").as_bytes());
            }
            byte => out.push(*byte),
        }
    }
    out.push(b'"');
    String::from_utf8(out).expect("only ASCII escapes were added")
}

fn needs_quoting(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| byte == b'\'' || byte == b'\r' || byte == b'\n' || invalid_ascii(byte))
}

/// Follows go-toml's `characters.InvalidAscii`: the control bytes that a
/// literal string may not carry. Tab, line feed and carriage return are not in
/// the table; `needs_quoting` rejects the two line breaks separately.
fn invalid_ascii(byte: u8) -> bool {
    byte <= 0x08 || byte == 0x0b || byte == 0x0c || (0x0e..=0x1f).contains(&byte) || byte == 0x7f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> SandboxConfig {
        SandboxConfig {
            network: Some("deny".to_string()),
            read_paths: vec!["/tmp/gh".to_string()],
            allow_env: vec!["GH_CONFIG_DIR".to_string()],
            driver: None,
        }
    }

    #[test]
    fn update_sandbox_rewrites_only_the_sandbox_table() {
        let cases = [
            "",
            "# keep\n[profiles.demo]\nmodel = '''a\n[sandbox]\nb'''\n",
            "# keep\n[sandbox] # old\nnetwork = 'allow'\nread_paths = [\n '/tmp/old',\n]\n[profiles.demo]\nmodel = 'keep'\n",
        ];
        for old in cases {
            let updated = update_sandbox(old.as_bytes(), &raw()).expect("update");
            let got = String::from_utf8(updated).expect("utf-8");
            assert!(got.contains("network = 'deny'"), "{got}");
            if let Some(index) = old.find("[profiles.demo]") {
                assert!(
                    got.contains(&old[index..]),
                    "unrelated content changed: {got}"
                );
            }
        }
    }

    #[test]
    fn update_sandbox_rejects_a_dotted_layout() {
        let error = update_sandbox(b"sandbox.network = 'allow'\n", &raw())
            .expect_err("must reject unsupported dotted layout without overwriting it");
        assert_eq!(
            error.to_string(),
            "setup requires a separate [sandbox] table; configuration was not changed"
        );
    }

    #[test]
    fn update_sandbox_writes_the_go_toml_compatible_block() {
        let settings = SandboxConfig {
            driver: Some("auto".to_string()),
            network: Some("allow".to_string()),
            read_paths: vec!["/tmp/it's".to_string()],
            allow_env: Vec::new(),
        };
        let updated = update_sandbox(b"", &settings).expect("update");
        assert_eq!(
            String::from_utf8(updated).expect("utf-8"),
            "\n[sandbox]\ndriver = 'auto'\nnetwork = 'allow'\nread_paths = [\"/tmp/it's\"]\nallow_env = []\n"
        );
    }

    #[test]
    fn update_sandbox_rejects_invalid_content() {
        let error = update_sandbox(b"not = toml =\n", &raw()).expect_err("invalid");
        assert_eq!(error.to_string(), "invalid configuration");
    }
}
