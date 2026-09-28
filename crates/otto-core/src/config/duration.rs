//! Minimal Go-`time.ParseDuration`-compatible parser for configuration.
//!
//! Supports signed sequences of decimal `{number}{unit}` terms (unit one of
//! ns/us/µs/ms/s/m/h) and returns nanoseconds. Arithmetic is checked so values
//! outside Go's signed 64-bit duration range are rejected rather than clamped.

/// Parses a Go-style duration and returns its signed nanosecond count.
pub fn parse_go_duration(input: &str) -> Result<i64, String> {
    let invalid = || format!("time: invalid duration {input:?}");
    if input.is_empty() {
        return Err(invalid());
    }

    let (negative, mut rest) = match input.as_bytes()[0] {
        b'-' => (true, &input[1..]),
        b'+' => (false, &input[1..]),
        _ => (false, input),
    };
    if rest == "0" {
        return Ok(0);
    }
    if rest.is_empty() {
        return Err(invalid());
    }

    let limit = if negative {
        (i64::MAX as u64) + 1
    } else {
        i64::MAX as u64
    };
    let mut total = 0_u64;
    while !rest.is_empty() {
        let whole_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let whole_text = &rest[..whole_end];
        rest = &rest[whole_end..];

        let mut fraction_text = "";
        if let Some(after_dot) = rest.strip_prefix('.') {
            let fraction_end = after_dot
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after_dot.len());
            fraction_text = &after_dot[..fraction_end];
            rest = &after_dot[fraction_end..];
        }
        if whole_text.is_empty() && fraction_text.is_empty() {
            return Err(invalid());
        }

        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        let unit_nanos = match unit {
            "ns" => 1_u64,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return Err(format!("time: unknown unit {unit:?} in duration {input:?}")),
        };

        let whole = parse_bounded_u64(whole_text, limit).ok_or_else(&invalid)?;
        let whole_nanos = whole.checked_mul(unit_nanos).ok_or_else(&invalid)?;
        let fraction_nanos = fractional_nanos(fraction_text, unit_nanos);
        let term = whole_nanos
            .checked_add(fraction_nanos)
            .ok_or_else(&invalid)?;
        total = total.checked_add(term).ok_or_else(&invalid)?;
        if total > limit {
            return Err(invalid());
        }
        rest = &rest[unit_end..];
    }

    if negative {
        if total == (i64::MAX as u64) + 1 {
            Ok(i64::MIN)
        } else {
            Ok(-(total as i64))
        }
    } else {
        Ok(total as i64)
    }
}

fn parse_bounded_u64(digits: &str, limit: u64) -> Option<u64> {
    let mut value = 0_u64;
    for byte in digits.bytes() {
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
        if value > limit {
            return None;
        }
    }
    Some(value)
}

fn fractional_nanos(digits: &str, unit_nanos: u64) -> u64 {
    // Units have at most 13 decimal digits; later digits cannot affect the
    // truncated nanosecond result. u128 keeps the bounded multiplication exact.
    let significant = digits.len().min(13);
    let mut numerator = 0_u64;
    let mut denominator = 1_u64;
    for byte in digits.bytes().take(significant) {
        numerator = numerator * 10 + u64::from(byte - b'0');
        denominator *= 10;
    }
    ((u128::from(numerator) * u128::from(unit_nanos)) / u128::from(denominator)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn parses_simple_compound_and_fractional_units() {
        assert_eq!(parse_go_duration("10s").unwrap(), 10_000_000_000);
        assert_eq!(parse_go_duration("0s").unwrap(), 0);
        assert_eq!(parse_go_duration("-1s").unwrap(), -1_000_000_000);
        assert_eq!(parse_go_duration("1h30m").unwrap(), 5_400_000_000_000);
        assert_eq!(parse_go_duration("1.5ms").unwrap(), 1_500_000);
        assert_eq!(parse_go_duration(".0000000000009h").unwrap(), 3);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_signed_nanosecond_boundaries() {
        assert_eq!(
            parse_go_duration("9223372036854775807ns").unwrap(),
            i64::MAX
        );
        assert_eq!(
            parse_go_duration("-9223372036854775808ns").unwrap(),
            i64::MIN
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_direct_compound_and_fractional_overflow() {
        for value in [
            "9223372036854775808ns",
            "-9223372036854775809ns",
            "9223372036854775807ns1ns",
            "2562047h47m16.854775808s",
            "-2562047h47m16.854775809s",
            "999999999999999999999999999999999999h",
        ] {
            assert!(parse_go_duration(value).is_err(), "accepted {value}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_garbage() {
        assert!(parse_go_duration("not-a-duration").is_err());
    }
}
