//! Minimal Go-`time.ParseDuration`-compatible parser for the duration
//! strings Otto accepts in `shell_timeout` and `sqlite.busy_timeout`.
//!
//! ponytail: supports the sign + `{number}{unit}` sequence Go accepts (unit
//! one of ns/us/µs/ms/s/m/h, decimal magnitudes, multiple terms like
//! "1h30m"). Returns nanoseconds as `i64`. Callers only need the failure to
//! be *an* error (Go's own error text is never asserted on directly — every
//! caller wraps it in a message like "invalid shell_timeout: ..." and only
//! that wrapper text is checked), so the message here is descriptive but not
//! byte-matched to Go. Doesn't reproduce Go's arbitrary-precision overflow
//! handling: config timeouts never approach `i64::MAX` nanoseconds.

pub(super) fn parse_go_duration(input: &str) -> Result<i64, String> {
    let invalid = || format!("time: invalid duration {input:?}");

    let mut s = input;
    if s.is_empty() {
        return Err(invalid());
    }
    let negative = match s.as_bytes()[0] {
        b'-' => {
            s = &s[1..];
            true
        }
        b'+' => {
            s = &s[1..];
            false
        }
        _ => false,
    };
    if s == "0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }

    let mut total = 0.0_f64;
    let mut consumed_any = false;
    while !s.is_empty() {
        let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let mut number_text = s[..digits_end].to_string();
        let mut rest = &s[digits_end..];
        if let Some(after_dot) = rest.strip_prefix('.') {
            let frac_end = after_dot
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after_dot.len());
            number_text.push('.');
            number_text.push_str(&after_dot[..frac_end]);
            rest = &after_dot[frac_end..];
        }
        if number_text.is_empty() || number_text == "." {
            return Err(invalid());
        }

        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        let unit_nanos = match unit {
            "ns" => 1.0,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000.0,
            "ms" => 1_000_000.0,
            "s" => 1_000_000_000.0,
            "m" => 60_000_000_000.0,
            "h" => 3_600_000_000_000.0,
            _ => return Err(format!("time: unknown unit {unit:?} in duration {input:?}")),
        };
        let number: f64 = number_text.parse().map_err(|_| invalid())?;
        total += number * unit_nanos;
        consumed_any = true;
        s = &rest[unit_end..];
    }
    if !consumed_any {
        return Err(invalid());
    }
    let nanos = total as i64;
    Ok(if negative { -nanos } else { nanos })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn parses_simple_and_compound_units() {
        assert_eq!(parse_go_duration("10s").unwrap(), 10_000_000_000);
        assert_eq!(parse_go_duration("0s").unwrap(), 0);
        assert_eq!(parse_go_duration("-1s").unwrap(), -1_000_000_000);
        assert_eq!(parse_go_duration("1h30m").unwrap(), 5_400_000_000_000);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_garbage() {
        assert!(parse_go_duration("not-a-duration").is_err());
    }
}
