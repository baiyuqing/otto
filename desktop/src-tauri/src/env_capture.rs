//! Parses the output of `<shell> -l -i -c 'printf OTTO_ENV_BEGIN; env -0'`:
//! a login/interactive shell can print rc-file banners to stdout before the
//! marker, so the parser looks for the marker and only reads what follows
//! it, split on the NUL bytes `env -0` uses instead of newlines (a value may
//! itself contain `=` or a newline).

const MARKER: &str = "OTTO_ENV_BEGIN";

/// Why [`parse_env_output`] could not recover an environment from `bytes`.
#[derive(Debug, PartialEq, Eq)]
pub enum EnvParseError {
    /// The marker never appeared, so rc-file output could not be
    /// distinguished from `env -0`'s output.
    MarkerNotFound,
}

/// Parses `bytes` into `(key, value)` pairs, in the order `env -0` printed
/// them. Bytes before and including `MARKER` are discarded; each
/// NUL-terminated entry after it is split on the first `=`. A trailing empty
/// entry (from the final NUL) is dropped. An entry with no `=` is skipped
/// (not valid `KEY=value` shape).
pub fn parse_env_output(bytes: &[u8]) -> Result<Vec<(String, String)>, EnvParseError> {
    let marker = MARKER.as_bytes();
    let marker_start = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or(EnvParseError::MarkerNotFound)?;
    let after_marker = &bytes[marker_start + marker.len()..];
    Ok(after_marker
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let equals = entry.iter().position(|byte| *byte == b'=')?;
            let key = String::from_utf8_lossy(&entry[..equals]).into_owned();
            let value = String::from_utf8_lossy(&entry[equals + 1..]).into_owned();
            Some((key, value))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_after_the_marker() {
        let mut input = b"rc file banner\n".to_vec();
        input.extend_from_slice(MARKER.as_bytes());
        input.extend_from_slice(b"PATH=/usr/bin\0HOME=/Users/me\0");
        let got = parse_env_output(&input).unwrap();
        assert_eq!(
            got,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("HOME".to_string(), "/Users/me".to_string()),
            ]
        );
    }

    #[test]
    fn keeps_a_value_containing_equals_or_newline() {
        let mut input = MARKER.as_bytes().to_vec();
        input.extend_from_slice(b"FOO=a=b\0BAR=line1\nline2\0");
        let got = parse_env_output(&input).unwrap();
        assert_eq!(
            got,
            vec![
                ("FOO".to_string(), "a=b".to_string()),
                ("BAR".to_string(), "line1\nline2".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_output_with_no_marker() {
        let got = parse_env_output(b"PATH=/usr/bin\0");
        assert_eq!(got, Err(EnvParseError::MarkerNotFound));
    }

    #[test]
    fn skips_an_entry_with_no_equals_sign() {
        let mut input = MARKER.as_bytes().to_vec();
        input.extend_from_slice(b"MALFORMED\0PATH=/usr/bin\0");
        let got = parse_env_output(&input).unwrap();
        assert_eq!(got, vec![("PATH".to_string(), "/usr/bin".to_string())]);
    }
}
