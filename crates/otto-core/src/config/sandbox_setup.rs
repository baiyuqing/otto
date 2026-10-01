//! Updating the `[sandbox]` table in an existing `config.toml`.
//!
//! `otto sandbox setup`, `/sandbox allow`, and `/sandbox network` change only
//! the four sandbox keys through [`super::edit::set_value`], so comments,
//! blank lines, and every other table stay byte-for-byte as they were.
//!
//! Values are encoded as go-toml v2 marshals them, the encoding existing
//! `config.toml` files were written with.

use super::{ConfigError, SandboxConfig, edit, resolve_sandbox};

/// Sets the `driver`, `network`, `read_paths`, and `allow_env` keys of the
/// `[sandbox]` table to `settings`, removing an absent optional key, and
/// appends a `[sandbox]` table when the document has none.
///
/// Errors: the content must already be a valid document, and an existing
/// sandbox table must be written as its own `[sandbox]` header rather than
/// dotted keys or an inline table.
pub fn update_sandbox(content: &[u8], settings: &SandboxConfig) -> Result<Vec<u8>, ConfigError> {
    resolve_sandbox(settings, None)?;
    let invalid = || ConfigError::new("invalid configuration");
    let text = std::str::from_utf8(content).map_err(|_| invalid())?;
    let parsed: toml::Table = toml::from_str(text).map_err(|_| invalid())?;
    let mut text = if parsed.contains_key("sandbox") {
        text.to_string()
    } else {
        format!("{text}\n[sandbox]\n")
    };
    let values = [
        ("driver", settings.driver.as_deref().map(encode_string)),
        ("network", settings.network.as_deref().map(encode_string)),
        ("read_paths", Some(encode_array(&settings.read_paths))),
        ("allow_env", Some(encode_array(&settings.allow_env))),
        // Written only when used, so files without exclusions keep their
        // existing four keys.
        (
            "excluded_commands",
            (!settings.excluded_commands.is_empty())
                .then(|| encode_array(&settings.excluded_commands)),
        ),
    ];
    for (key, value) in values {
        text = edit::set_value(&text, &["sandbox"], key, value.as_deref())?;
    }
    Ok(text.into_bytes())
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
            excluded_commands: Vec::new(),
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
    fn update_sandbox_keeps_comments_in_and_after_the_sandbox_table() {
        let old = "[sandbox] # boundary\n# driver = 'off'  # only for debugging\nnetwork = 'allow' # default\nread_paths = []\n\n# profiles follow\n[profiles.demo]\nmodel = 'keep'\n";
        let updated = update_sandbox(old.as_bytes(), &raw()).expect("update");
        assert_eq!(
            String::from_utf8(updated).expect("utf-8"),
            "[sandbox] # boundary\n# driver = 'off'  # only for debugging\nnetwork = 'deny' # default\nread_paths = ['/tmp/gh']\nallow_env = ['GH_CONFIG_DIR']\n\n# profiles follow\n[profiles.demo]\nmodel = 'keep'\n"
        );
    }

    #[test]
    fn update_sandbox_rejects_a_dotted_layout() {
        let error = update_sandbox(b"sandbox.network = 'allow'\n", &raw())
            .expect_err("must reject unsupported dotted layout without overwriting it");
        assert!(
            error.to_string().contains("configuration was not changed"),
            "{error}"
        );
    }

    #[test]
    fn update_sandbox_writes_the_go_toml_compatible_block() {
        let settings = SandboxConfig {
            driver: Some("auto".to_string()),
            network: Some("allow".to_string()),
            read_paths: vec!["/tmp/it's".to_string()],
            allow_env: Vec::new(),
            excluded_commands: Vec::new(),
        };
        let updated = update_sandbox(b"", &settings).expect("update");
        assert_eq!(
            String::from_utf8(updated).expect("utf-8"),
            "\n[sandbox]\ndriver = 'auto'\nnetwork = 'allow'\nread_paths = [\"/tmp/it's\"]\nallow_env = []\n"
        );
    }

    #[test]
    fn update_sandbox_writes_excluded_commands_only_when_present() {
        let settings = SandboxConfig {
            excluded_commands: vec!["lark-cli *".to_string(), "tool 'a b' *".to_string()],
            ..raw()
        };
        let updated = update_sandbox(b"[sandbox]\nnetwork = 'allow'\n", &settings).expect("update");
        let got = String::from_utf8(updated).expect("utf-8");
        assert!(
            got.ends_with("excluded_commands = ['lark-cli *', \"tool 'a b' *\"]\n"),
            "{got}"
        );
        let parsed: SandboxConfig = toml::from_str::<toml::Table>(&got).expect("toml")["sandbox"]
            .clone()
            .try_into()
            .expect("sandbox");
        assert_eq!(parsed.excluded_commands, settings.excluded_commands);

        let invalid = SandboxConfig {
            excluded_commands: vec!["a; b".to_string()],
            ..raw()
        };
        assert!(update_sandbox(b"", &invalid).is_err());
    }

    #[test]
    fn update_sandbox_rejects_invalid_content() {
        let error = update_sandbox(b"not = toml =\n", &raw()).expect_err("invalid");
        assert_eq!(error.to_string(), "invalid configuration");
    }
}
