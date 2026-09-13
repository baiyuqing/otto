//! Conservative extraction of URL userinfo that must be treated as private at
//! process and provider boundaries.
//!
//! Port of Go's `internal/urlprivacy`. A proxy environment variable such as
//! `HTTPS_PROXY=http://user:pass@host` embeds a credential that has to be
//! redacted from transcripts and withheld from sandboxed children. This module
//! answers two questions about one value: which byte sequences are credential
//! material, and whether that extraction can be proven complete.
//!
//! Everything here is a pure function over borrowed bytes. There is no shared
//! state, no I/O, and nothing to cancel. Bytes rather than `str` because
//! percent escapes routinely decode to invalid UTF-8; returned values are
//! canonicalized to valid UTF-8 by [`crate::safetext::canonicalize_utf8`].

use std::collections::HashSet;

use crate::gourl;
use crate::safetext;

/// Values longer than this are reported as ambiguous without being scanned, so
/// that a pathological environment variable cannot drive the quadratic-looking
/// candidate scan below.
const MAX_USERINFO_INPUT_BYTES: usize = 16 << 10;

/// Returns raw and independently decoded userinfo components of `raw`.
///
/// The second element reports that extraction cannot be proven complete: the
/// caller must then treat the whole value as unredactable rather than trust the
/// returned forms. Values without a literal `@` are complete for this
/// credential detector even when they are not usable proxy URLs, so a
/// malformed but credential-free proxy setting stays usable.
pub fn userinfo_forms(raw: &[u8]) -> (Vec<String>, bool) {
    if raw.is_empty() || !raw.contains(&b'@') {
        return (Vec::new(), false);
    }
    if raw.len() > MAX_USERINFO_INPUT_BYTES {
        return (Vec::new(), true);
    }

    let mut collector = UserinfoCollector::new();
    let has_backslash = raw.contains(&b'\\');

    let Some(authority) = normal_url_authority(raw) else {
        collector.add_candidates(lexical_userinfo_candidates(raw));
        return (collector.values, true);
    };

    match authority.iter().filter(|&&b| b == b'@').count() {
        0 => {
            if has_backslash || !parseable_authority(authority) {
                collector.add_candidates(lexical_userinfo_candidates(raw));
                return (collector.values, true);
            }
            let ambiguous =
                gourl::parse(raw).is_err() || !valid_percent_escapes(raw) || has_backslash;
            (Vec::new(), ambiguous)
        }
        1 => {
            let userinfo_end = authority.iter().position(|&b| b == b'@').unwrap_or(0);
            if !collector.add_forms(&authority[..userinfo_end]) {
                return (collector.values, true);
            }
            let mut with_slashes = Vec::with_capacity(authority.len() + 2);
            with_slashes.extend_from_slice(b"//");
            with_slashes.extend_from_slice(authority);
            let authority_url = gourl::parse(&with_slashes);
            let host_empty = match &authority_url {
                Ok(url) => url.host.is_empty(),
                Err(()) => true,
            };
            if let Ok(url) = &authority_url
                && !url.host.is_empty()
                && !collector.add_parsed_userinfo_forms(url.user.as_ref())
            {
                return (collector.values, true);
            }
            let ambiguous = host_empty
                || gourl::parse(raw).is_err()
                || !valid_percent_escapes(raw)
                || has_backslash;
            if has_backslash && !collector.add_candidates(lexical_userinfo_candidates(raw)) {
                return (collector.values, true);
            }
            (collector.values, ambiguous)
        }
        _ => {
            if collector.add_candidates(authority) && has_backslash {
                collector.add_candidates(lexical_userinfo_candidates(raw));
            }
            (collector.values, true)
        }
    }
}

/// The authority of `raw` when it has the shape of a normal URL, that is
/// `scheme://authority...` or `//authority...` with a non-empty authority.
fn normal_url_authority(raw: &[u8]) -> Option<&[u8]> {
    let start = if raw.starts_with(b"//") {
        2
    } else {
        let separator = find(raw, b"://")?;
        if separator == 0 || !valid_scheme(&raw[..separator]) {
            return None;
        }
        separator + 3
    };
    if start >= raw.len() || raw[start] == b'/' || raw[start] == b'\\' {
        return None;
    }
    let end = match raw[start..]
        .iter()
        .position(|b| matches!(b, b'/' | b'?' | b'#' | b'\\'))
    {
        Some(offset) => start + offset,
        None => raw.len(),
    };
    if end == start {
        return None;
    }
    Some(&raw[start..end])
}

fn parseable_authority(authority: &[u8]) -> bool {
    let mut with_slashes = Vec::with_capacity(authority.len() + 2);
    with_slashes.extend_from_slice(b"//");
    with_slashes.extend_from_slice(authority);
    matches!(gourl::parse(&with_slashes), Ok(url) if !url.host.is_empty())
}

/// The span of `raw` that a lexical reader would treat as authority-like: after
/// any scheme separator and leading slashes, up to the first `?` or `#`.
fn lexical_userinfo_candidates(raw: &[u8]) -> &[u8] {
    let end = raw
        .iter()
        .position(|b| matches!(b, b'?' | b'#'))
        .unwrap_or(raw.len());
    if end == 0 {
        return b"";
    }
    let head = &raw[..end];

    let mut start = if let Some(separator) = find(head, b"://") {
        separator + 3
    } else if head.starts_with(b"//") {
        2
    } else if let Some(colon) = head.iter().position(|&b| b == b':') {
        if colon + 1 < end && (head[colon + 1] == b'/' || head[colon + 1] == b'\\') {
            colon + 1
        } else {
            0
        }
    } else {
        0
    };
    while start < end && (raw[start] == b'/' || raw[start] == b'\\') {
        start += 1;
    }
    if start >= end {
        return b"";
    }
    &raw[start..end]
}

/// Every `@`-terminated segment of `raw` that could be userinfo, plus the final
/// full prefix when the last `@` was preceded by a separator. Intermediate
/// cumulative prefixes are deliberately dropped: only the longest one can be a
/// single credential.
fn userinfo_candidate_segments(raw: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let mut candidates: Vec<&[u8]> = Vec::new();
    let mut final_before_at: &[u8] = b"";
    let mut offset = 0usize;
    while offset < raw.len() {
        let Some(relative_at) = raw[offset..].iter().position(|&b| b == b'@') else {
            break;
        };
        let at = offset + relative_at;
        let before_at = &raw[..at];
        let local_start = before_at
            .iter()
            .rposition(|b| matches!(b, b'/' | b'\\' | b'@'))
            .map_or(0, |index| index + 1);
        append_userinfo_candidate(&mut candidates, &before_at[local_start..]);
        if local_start > 0 {
            final_before_at = before_at;
        }
        offset = at + 1;
    }
    (candidates, final_before_at)
}

fn append_userinfo_candidate<'a>(candidates: &mut Vec<&'a [u8]>, candidate: &'a [u8]) {
    if candidate.is_empty() {
        return;
    }
    candidates.push(candidate);
    if let Some(colon) = candidate.iter().position(|&b| b == b':')
        && colon > 0
        && valid_scheme(&candidate[..colon])
    {
        let alternative = trim_start_slashes(&candidate[colon + 1..]);
        if !alternative.is_empty() {
            candidates.push(alternative);
        }
    }
}

fn trim_start_slashes(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|b| !matches!(b, b'/' | b'\\'))
        .unwrap_or(value.len());
    &value[start..]
}

/// Deduplicating, bounded accumulator for candidate secret forms. The bounds
/// are shared with [`crate::safetext`] so that a value rejected here is also one
/// the redactor would have refused.
struct UserinfoCollector {
    values: Vec<String>,
    seen: HashSet<String>,
    bytes: usize,
}

impl UserinfoCollector {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            seen: HashSet::new(),
            bytes: 0,
        }
    }

    /// Records one form. Returns false once a bound is reached, which the
    /// callers translate into an ambiguous result.
    fn add(&mut self, value: &[u8]) -> bool {
        let value = safetext::canonicalize_utf8(value);
        if value.is_empty() || self.seen.contains(&value) {
            return true;
        }
        if self.values.len() >= safetext::MAX_SECRET_VALUES
            || self.bytes > safetext::MAX_SECRET_BYTES - value.len()
        {
            return false;
        }
        self.bytes += value.len();
        self.seen.insert(value.clone());
        self.values.push(value);
        true
    }

    fn add_candidates(&mut self, raw: &[u8]) -> bool {
        let (locals, final_before_at) = userinfo_candidate_segments(raw);
        for candidate in locals {
            if !self.add_forms(candidate) {
                return false;
            }
        }
        if !final_before_at.is_empty() && !self.add(final_before_at) {
            return false;
        }
        true
    }

    /// Records a raw userinfo string together with its username, password, and
    /// independently percent-decoded variants.
    fn add_forms(&mut self, raw_userinfo: &[u8]) -> bool {
        if !self.add(raw_userinfo) {
            return false;
        }
        let (raw_username, raw_password) = match raw_userinfo.iter().position(|&b| b == b':') {
            Some(colon) => (&raw_userinfo[..colon], Some(&raw_userinfo[colon + 1..])),
            None => (raw_userinfo, None),
        };
        if !self.add(raw_username) {
            return false;
        }
        if let Ok(decoded) = gourl::path_unescape(raw_username)
            && !self.add(&decoded)
        {
            return false;
        }
        if let Some(raw_password) = raw_password {
            if !self.add(raw_password) {
                return false;
            }
            if let Ok(decoded) = gourl::path_unescape(raw_password)
                && !self.add(&decoded)
            {
                return false;
            }
        }
        if let Ok(decoded) = gourl::path_unescape(raw_userinfo)
            && !self.add(&decoded)
        {
            return false;
        }
        true
    }

    /// Records the forms Go's URL parser produced, including its re-encoded
    /// `username:password` rendering.
    fn add_parsed_userinfo_forms(&mut self, user: Option<&gourl::Userinfo>) -> bool {
        let Some(user) = user else {
            return true;
        };
        let username = user.username();
        if !self.add(username) {
            return false;
        }
        let mut decoded_userinfo = username.to_vec();
        if let Some(password) = user.password() {
            if !self.add(password) {
                return false;
            }
            decoded_userinfo.push(b':');
            decoded_userinfo.extend_from_slice(password);
        }
        self.add(&decoded_userinfo) && self.add(&user.encoded())
    }
}

fn valid_scheme(scheme: &[u8]) -> bool {
    match scheme.first() {
        None => false,
        Some(first) if !first.is_ascii_alphabetic() => false,
        Some(_) => scheme[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')),
    }
}

fn valid_percent_escapes(raw: &[u8]) -> bool {
    let mut index = 0usize;
    while index < raw.len() {
        if raw[index] == b'%' {
            if index + 2 >= raw.len()
                || !raw[index + 1].is_ascii_hexdigit()
                || !raw[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 2;
        }
        index += 1;
    }
    true
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::userinfo_forms;

    /// Runs one case of Go's
    /// `TestUserinfoFormsDistinguishesAuthorityFromPathQueryAndFragment`.
    #[track_caller]
    fn check(raw: &str, want: &[&str], want_ambiguous: bool) {
        let (values, ambiguous) = userinfo_forms(raw.as_bytes());
        assert_eq!(
            ambiguous, want_ambiguous,
            "ambiguity for {raw:?}; values = {values:?}"
        );
        for wanted in want {
            assert!(
                values.iter().any(|value| value == wanted),
                "{raw:?} omitted {wanted:?}: {values:?}"
            );
        }
        if want.is_empty() {
            assert!(
                values.is_empty(),
                "{raw:?} misclassified non-authority userinfo: {values:?}"
            );
        }
    }

    #[test]
    fn authority_userinfo_is_distinguished_from_path_query_and_fragment() {
        let raw_and_decoded = &[
            "raw%20user:raw%2Fpass",
            "raw%20user",
            "raw%2Fpass",
            "raw user:raw/pass",
            "raw user",
            "raw/pass",
        ];
        check(
            "https://raw%20user:raw%2Fpass@[2001:db8::1]:8443/path?next=@ignored#@ignored",
            raw_and_decoded,
            false,
        );
        check(
            "//raw%20user:raw%2Fpass@[2001:db8::1]:8443/path@ignored?next=@ignored#@ignored",
            raw_and_decoded,
            false,
        );
        check(
            "//[2001:db8::1]:8443/path/user:pass@example.test?next=user:pass@example.test#user:pass@example.test",
            &[],
            false,
        );
        check(
            "https://[2001:db8::1]:8443/path/user:pass@example.test?next=user:pass@example.test#user:pass@example.test",
            &[],
            false,
        );
    }

    #[test]
    fn malformed_authorities_stay_ambiguous_and_keep_their_candidates() {
        check(
            "https://[2001:db8::1]:8443/path/user:pass@example.test?broken=%zz",
            &[],
            true,
        );
        check(
            "https://bad%zz:pass%2Fword@[::1]:8443/path/user:other@example.test",
            &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
            true,
        );
        check(
            "https://[::1/path/raw%20user:raw%2Fpass@example.test",
            &[
                "raw%20user:raw%2Fpass",
                "raw%20user",
                "raw%2Fpass",
                "raw user",
                "raw/pass",
            ],
            true,
        );
        check(
            "https:///raw%20user:raw%2Fpass@example.test/path",
            &[
                "raw%20user:raw%2Fpass",
                "raw%20user",
                "raw%2Fpass",
                "raw user:raw/pass",
                "raw user",
                "raw/pass",
            ],
            true,
        );
        check(
            "raw%20user:raw%2Fpass@example.test/path",
            &[
                "raw%20user:raw%2Fpass",
                "raw%20user",
                "raw%2Fpass",
                "raw user:raw/pass",
                "raw user",
                "raw/pass",
            ],
            true,
        );
        check(
            "https://bad%zz:pass%2Fword@[::1]:8443/path",
            &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
            true,
        );
    }

    #[test]
    fn backslash_authorities_are_read_lexically() {
        check(
            r"https:\\raw%20user:raw%2Fpass@example.test\path",
            &[
                "raw%20user:raw%2Fpass",
                "raw%20user",
                "raw%2Fpass",
                "raw user:raw/pass",
                "raw user",
                "raw/pass",
            ],
            true,
        );
        check(
            r"https://host\raw%20user:raw%2Fpass@example.test",
            &[
                "raw%20user:raw%2Fpass",
                "raw user:raw/pass",
                "raw user",
                "raw/pass",
            ],
            true,
        );
        check(
            r"https://first:first-pass@host\second%20user:second%2Fpass@example.test",
            &[
                "first:first-pass",
                "first",
                "first-pass",
                "second%20user:second%2Fpass",
                "second user:second/pass",
            ],
            true,
        );
    }

    #[test]
    fn multiple_at_signs_keep_every_plausible_candidate() {
        check(
            "http:///real%20user:real%2Fpass@proxy/path%20user:path%2Fpass@example",
            &[
                "real%20user:real%2Fpass",
                "real user:real/pass",
                "path%20user:path%2Fpass",
                "path user:path/pass",
            ],
            true,
        );
        check(
            "missing://first%20user:first%2Fpass@proxy@host",
            &[
                "first%20user:first%2Fpass",
                "first user:first/pass",
                "proxy",
            ],
            true,
        );
    }

    #[test]
    fn every_proxy_shape_without_an_at_sign_is_preserved() {
        for raw in [
            "https://example.test/path%zz",
            "https:///example.test/path",
            r"https:\\example.test\path",
            "example.test:8443",
            "//example.test:8443",
            "http://:8080",
            "http://example.test:99999",
        ] {
            let (values, ambiguous) = userinfo_forms(raw.as_bytes());
            assert!(
                !ambiguous && values.is_empty(),
                "{raw:?} = {values:?}, ambiguous {ambiguous}; want complete extraction of no forms"
            );
        }
    }

    #[test]
    fn every_independently_decoded_component_is_canonicalized() {
        let (values, ambiguous) = userinfo_forms(b"https://user%FF:pass%C0%AF@[::1]:8443");
        assert!(!ambiguous, "valid percent escapes classified as malformed");
        for wanted in [
            "user%FF:pass%C0%AF",
            "user%FF",
            "pass%C0%AF",
            "user\u{fffd}:pass\u{fffd}\u{fffd}",
            "user\u{fffd}",
            "pass\u{fffd}\u{fffd}",
        ] {
            assert!(
                values.iter().any(|value| value == wanted),
                "omitted {wanted:?}: {values:?}"
            );
        }
    }

    #[test]
    fn a_malformed_multi_at_value_keeps_only_the_final_full_prefix() {
        let raw = b"http:///first:one@mid/second:two@tail/third:three@example";
        let (values, ambiguous) = userinfo_forms(raw);
        assert!(ambiguous, "expected ambiguity; values = {values:?}");
        for wanted in [
            "first:one",
            "second:two",
            "third:three",
            "first:one@mid/second:two@tail/third:three",
        ] {
            assert!(
                values.iter().any(|value| value == wanted),
                "omitted {wanted:?}: {values:?}"
            );
        }
        assert!(
            !values
                .iter()
                .any(|value| value == "first:one@mid/second:two"),
            "retained an intermediate cumulative prefix: {values:?}"
        );
    }
}
