//! Go-compatible canonical JSON for the small value shapes the store persists.
//!
//! `serde_json` is not interchangeable here: Go's `encoding/json` escapes `<`,
//! `>`, `&`, U+2028 and U+2029 by default, so the same map produces different
//! bytes. The store compares a decoded blob against its re-encoded form to
//! detect corruption, and the Go and Rust binaries share one database file, so
//! the encoder has to agree byte for byte.

use std::collections::BTreeMap;

/// Go's `encoding/json` float encoding: shortest round-trip decimal, switching
/// to exponent form only below 1e-6 or at/above 1e21. Rust's `Display` already
/// produces the shortest round-trip decimal and never uses an exponent, so the
/// two agree once the exponent range is handled explicitly.
pub fn encode_float(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude != 0.0 && (magnitude < 1e-6 || magnitude >= 1e21) {
        return format!("{value:e}");
    }
    format!("{value}")
}

pub fn encode_string(value: &str, out: &mut String) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            control if control < '\u{20}' => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Encodes a `[]string`. A Go nil slice marshals as `null`, so callers that
/// need `[]` pass an empty slice; this encoder always emits an array.
pub fn encode_string_slice(values: &[String]) -> String {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        encode_string(value, &mut out);
    }
    out.push(']');
    out
}

/// Encodes a `map[string]string`. Go sorts map keys when marshaling, which
/// `BTreeMap` iteration already does.
pub fn encode_string_map(values: &BTreeMap<String, String>) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        encode_string(key, &mut out);
        out.push(':');
        encode_string(value, &mut out);
    }
    out.push('}');
    out
}

/// Decodes a JSON array of strings, rejecting trailing content. Returns `None`
/// for anything that is not exactly one array of strings.
pub fn decode_string_slice(raw: &str) -> Option<Vec<String>> {
    serde_json::from_str::<Vec<String>>(raw).ok()
}

/// Decodes a JSON object of string values, rejecting trailing content.
pub fn decode_string_map(raw: &str) -> Option<BTreeMap<String, String>> {
    serde_json::from_str::<BTreeMap<String, String>>(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_shapes_encode_as_empty_containers() {
        assert_eq!(encode_string_slice(&[]), "[]");
        assert_eq!(encode_string_map(&BTreeMap::new()), "{}");
    }

    #[test]
    fn html_characters_use_go_escapes() {
        let encoded = encode_string_slice(&["a<b>c&d".to_string()]);
        assert_eq!(encoded, r#"["a\u003cb\u003ec\u0026d"]"#);
    }

    #[test]
    fn map_keys_are_sorted_and_round_trip() {
        let mut values = BTreeMap::new();
        values.insert("b".to_string(), "2".to_string());
        values.insert("a".to_string(), "1\"q".to_string());
        let encoded = encode_string_map(&values);
        assert_eq!(encoded, r#"{"a":"1\"q","b":"2"}"#);
        assert_eq!(decode_string_map(&encoded).expect("decode"), values);
    }

    #[test]
    fn decoding_rejects_wrong_shapes() {
        assert!(decode_string_slice("[1]").is_none());
        assert!(decode_string_slice("[] []").is_none());
        assert!(decode_string_map(r#"{"a":1}"#).is_none());
    }
}
