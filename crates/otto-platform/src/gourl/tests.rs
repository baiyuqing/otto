//! Direct tests for [`super`] (`gourl.rs`) against vectors produced by Go.
//!
//! Go version that produced every vector: go1.27.3 darwin/arm64 (`go version`).
//! The expected values are what the Go standard library `net/url` returns, not
//! what this crate returns.
//!
//! Go functions called (a throwaway program outside the repository; no `.go`
//! file is committed):
//!
//! - Parse rows (`parse_tests`): `url.Parse(input)`. On error the row is
//!   `err(input)`. On success the row holds `Scheme`, `Opaque`,
//!   `User.Username()`, `User.Password()` with its set flag, `User.String()`,
//!   `Host`, `Path`, `RawQuery` and `Fragment`; these are the fields of
//!   [`super::Url`]. `RawPath`, `RawFragment`, `ForceQuery`, `Hostname()` and
//!   `Port()` are not stored by the Rust `Url`.
//! - Escape tables: `ESCAPE_PATH_SEGMENT` uses `url.PathEscape`,
//!   `ESCAPE_QUERY_COMPONENT` uses `url.QueryEscape`. Go has no public
//!   function for the other modes, so they come from the public method that
//!   calls Go's internal `escape` with that mode: `URL.EscapedPath()` (path),
//!   `url.User(s).String()` (user password), `URL.String()` of a URL with
//!   scheme `x` and `Host` set, minus the `x://` prefix (host), and
//!   `URL.EscapedFragment()` (fragment). Go does not call `escape` with the
//!   zone mode, so [`super::Encoding::Zone`] has no escape vector.
//! - Unescape tables: `url.PathUnescape` and `url.QueryUnescape`
//!   are public. The path, host, zone, user password and fragment modes are
//!   derived through `url.Parse` of a URL that places the input in that
//!   component; each table's `// Go:` comment names the URL and the field read.
//!   Derived inputs contain none of the delimiters (`/ ? # @ :`) that would
//!   change which component the input falls in.
//!
//! To regenerate the vectors, rewrite that program: for each input string
//! (arbitrary bytes), call the function named above and print the input and
//! the result (or the error marker) as a Rust byte-string row, then paste the
//! rows into the tables below. Rows whose Rust result differs from Go are kept
//! in `KNOWN_DIFFERENCES` in the parse tests, asserting the current Rust
//! result with the Go result in a comment.

use super::{Encoding, escape, path_unescape, unescape};

#[path = "parse_tests.rs"]
mod parse_tests;

fn show(bytes: &[u8]) -> String {
    let escaped: String = bytes
        .iter()
        .flat_map(|b| std::ascii::escape_default(*b))
        .map(char::from)
        .collect();
    format!("b\"{escaped}\"")
}

fn check_escape(table: &[(&[u8], &[u8])], mode: Encoding) {
    let mut failures = Vec::new();
    for (input, want) in table {
        let got = escape(input, mode);
        if got != *want {
            failures.push(format!(
                "{} => got {} want {}",
                show(input),
                show(&got),
                show(want)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{mode:?} escape differs from Go:\n{}",
        failures.join("\n")
    );
}

fn check_unescape(table: &[(&[u8], Option<&[u8]>)], mode: Encoding) {
    let mut failures = Vec::new();
    for (input, want) in table {
        let got = unescape(input, mode).ok();
        if got.as_deref() != *want {
            let got = got.as_deref().map_or_else(|| "Err".to_string(), show);
            let want = want.map_or_else(|| "Err".to_string(), show);
            failures.push(format!("{} => got {got} want {want}", show(input)));
        }
    }
    assert!(
        failures.is_empty(),
        "{mode:?} unescape differs from Go:\n{}",
        failures.join("\n")
    );
}

#[test]
fn escape_path_matches_go() {
    check_escape(ESCAPE_PATH, Encoding::Path);
}

#[test]
fn escape_path_segment_matches_go() {
    check_escape(ESCAPE_PATH_SEGMENT, Encoding::PathSegment);
}

#[test]
fn escape_query_component_matches_go() {
    check_escape(ESCAPE_QUERY_COMPONENT, Encoding::QueryComponent);
}

#[test]
fn escape_user_password_matches_go() {
    check_escape(ESCAPE_USER_PASSWORD, Encoding::UserPassword);
}

#[test]
fn escape_host_matches_go() {
    check_escape(ESCAPE_HOST, Encoding::Host);
}

#[test]
fn escape_fragment_matches_go() {
    check_escape(ESCAPE_FRAGMENT, Encoding::Fragment);
}

#[test]
fn unescape_path_segment_matches_go() {
    check_unescape(UNESCAPE_PATH_SEGMENT, Encoding::PathSegment);
}

#[test]
fn path_unescape_is_path_segment_mode() {
    for (input, want) in UNESCAPE_PATH_SEGMENT {
        assert_eq!(
            path_unescape(input).ok().as_deref(),
            *want,
            "{}",
            show(input)
        );
    }
}

#[test]
fn unescape_query_component_matches_go() {
    check_unescape(UNESCAPE_QUERY_COMPONENT, Encoding::QueryComponent);
}

#[test]
fn unescape_path_matches_go() {
    check_unescape(UNESCAPE_PATH, Encoding::Path);
}

#[test]
fn unescape_host_matches_go() {
    check_unescape(UNESCAPE_HOST, Encoding::Host);
}

#[test]
fn unescape_zone_matches_go() {
    check_unescape(UNESCAPE_ZONE, Encoding::Zone);
}

#[test]
fn unescape_user_password_matches_go() {
    check_unescape(UNESCAPE_USER_PASSWORD, Encoding::UserPassword);
}

#[test]
fn unescape_fragment_matches_go() {
    check_unescape(UNESCAPE_FRAGMENT, Encoding::Fragment);
}

// Go: (&url.URL{Path: s}).EscapedPath()
const ESCAPE_PATH: &[(&[u8], &[u8])] = &[
    (b"", b""),
    (b"abc", b"abc"),
    (b"a b", b"a%20b"),
    (b"a/b", b"a/b"),
    (b"a?b", b"a%3Fb"),
    (b"a:b@c", b"a:b@c"),
    (b"a;b,c", b"a;b,c"),
    (b"a+b=c&d$e", b"a+b=c&d$e"),
    (b"-_.~!*'()", b"-_.~%21%2A%27%28%29"),
    (b"%", b"%25"),
    (b"\xc3\xa9", b"%C3%A9"),
    (b"\x00\xff\x7f", b"%00%FF%7F"),
    (b"[x]<>\"", b"%5Bx%5D%3C%3E%22"),
    (b"#", b"%23"),
    (b"\\", b"%5C"),
    (b"{}|^`", b"%7B%7D%7C%5E%60"),
    (b"A~z09", b"A~z09"),
];

// Go: url.PathEscape(s)
const ESCAPE_PATH_SEGMENT: &[(&[u8], &[u8])] = &[
    (b"", b""),
    (b"abc", b"abc"),
    (b"a b", b"a%20b"),
    (b"a/b", b"a%2Fb"),
    (b"a?b", b"a%3Fb"),
    (b"a:b@c", b"a:b@c"),
    (b"a;b,c", b"a%3Bb%2Cc"),
    (b"a+b=c&d$e", b"a+b=c&d$e"),
    (b"-_.~!*'()", b"-_.~%21%2A%27%28%29"),
    (b"%", b"%25"),
    (b"\xc3\xa9", b"%C3%A9"),
    (b"\x00\xff\x7f", b"%00%FF%7F"),
    (b"[x]<>\"", b"%5Bx%5D%3C%3E%22"),
    (b"#", b"%23"),
    (b"\\", b"%5C"),
    (b"{}|^`", b"%7B%7D%7C%5E%60"),
    (b"A~z09", b"A~z09"),
];

// Go: url.QueryEscape(s)
const ESCAPE_QUERY_COMPONENT: &[(&[u8], &[u8])] = &[
    (b"", b""),
    (b"abc", b"abc"),
    (b"a b", b"a+b"),
    (b"a/b", b"a%2Fb"),
    (b"a?b", b"a%3Fb"),
    (b"a:b@c", b"a%3Ab%40c"),
    (b"a;b,c", b"a%3Bb%2Cc"),
    (b"a+b=c&d$e", b"a%2Bb%3Dc%26d%24e"),
    (b"-_.~!*'()", b"-_.~%21%2A%27%28%29"),
    (b"%", b"%25"),
    (b"\xc3\xa9", b"%C3%A9"),
    (b"\x00\xff\x7f", b"%00%FF%7F"),
    (b"[x]<>\"", b"%5Bx%5D%3C%3E%22"),
    (b"#", b"%23"),
    (b"\\", b"%5C"),
    (b"{}|^`", b"%7B%7D%7C%5E%60"),
    (b"A~z09", b"A~z09"),
];

// Go: url.User(s).String()
const ESCAPE_USER_PASSWORD: &[(&[u8], &[u8])] = &[
    (b"", b""),
    (b"abc", b"abc"),
    (b"a b", b"a%20b"),
    (b"a/b", b"a%2Fb"),
    (b"a?b", b"a%3Fb"),
    (b"a:b@c", b"a%3Ab%40c"),
    (b"a;b,c", b"a;b,c"),
    (b"a+b=c&d$e", b"a+b=c&d$e"),
    (b"-_.~!*'()", b"-_.~%21%2A%27%28%29"),
    (b"%", b"%25"),
    (b"\xc3\xa9", b"%C3%A9"),
    (b"\x00\xff\x7f", b"%00%FF%7F"),
    (b"[x]<>\"", b"%5Bx%5D%3C%3E%22"),
    (b"#", b"%23"),
    (b"\\", b"%5C"),
    (b"{}|^`", b"%7B%7D%7C%5E%60"),
    (b"A~z09", b"A~z09"),
];

// Go: (&url.URL{Scheme: "x", Host: s}).String() without the "x://" prefix
const ESCAPE_HOST: &[(&[u8], &[u8])] = &[
    (b"abc", b"abc"),
    (b"a b", b"a%20b"),
    (b"a/b", b"a%2Fb"),
    (b"a?b", b"a%3Fb"),
    (b"a:b@c", b"a:b%40c"),
    (b"a;b,c", b"a;b,c"),
    (b"a+b=c&d$e", b"a+b=c&d$e"),
    (b"-_.~!*'()", b"-_.~!*'()"),
    (b"%", b"%25"),
    (b"\xc3\xa9", b"%C3%A9"),
    (b"\x00\xff\x7f", b"%00%FF%7F"),
    (b"[x]<>\"", b"[x]<>\""),
    (b"#", b"%23"),
    (b"\\", b"%5C"),
    (b"{}|^`", b"%7B%7D%7C%5E%60"),
    (b"A~z09", b"A~z09"),
];

// Go: (&url.URL{Fragment: s}).EscapedFragment()
const ESCAPE_FRAGMENT: &[(&[u8], &[u8])] = &[
    (b"", b""),
    (b"abc", b"abc"),
    (b"a b", b"a%20b"),
    (b"a/b", b"a/b"),
    (b"a?b", b"a?b"),
    (b"a:b@c", b"a:b@c"),
    (b"a;b,c", b"a;b,c"),
    (b"a+b=c&d$e", b"a+b=c&d$e"),
    (b"-_.~!*'()", b"-_.~!*%27()"),
    (b"%", b"%25"),
    (b"\xc3\xa9", b"%C3%A9"),
    (b"\x00\xff\x7f", b"%00%FF%7F"),
    (b"[x]<>\"", b"%5Bx%5D%3C%3E%22"),
    (b"#", b"%23"),
    (b"\\", b"%5C"),
    (b"{}|^`", b"%7B%7D%7C%5E%60"),
    (b"A~z09", b"A~z09"),
];

// Go: url.PathUnescape(s)
const UNESCAPE_PATH_SEGMENT: &[(&[u8], Option<&[u8]>)] = &[
    (b"", Some(b"")),
    (b"abc", Some(b"abc")),
    (b"a+b", Some(b"a+b")),
    (b"a%20b", Some(b"a b")),
    (b"a%2Bb", Some(b"a+b")),
    (b"%41%42", Some(b"AB")),
    (b"%", None),
    (b"%4", None),
    (b"%zz", None),
    (b"%4g", None),
    (b"%e4%bd%a0", Some(b"\xe4\xbd\xa0")),
    (b"%ff", Some(b"\xff")),
    (b"%00", Some(b"\x00")),
    (b"a%2", None),
    (b"%%41", None),
    (b"%25", Some(b"%")),
    (b"%2f", Some(b"/")),
    (b"%3D%3d", Some(b"==")),
    (b"%7e%7E", Some(b"~~")),
    (b"a\xffb", Some(b"a\xffb")),
];

// Go: url.QueryUnescape(s)
const UNESCAPE_QUERY_COMPONENT: &[(&[u8], Option<&[u8]>)] = &[
    (b"", Some(b"")),
    (b"abc", Some(b"abc")),
    (b"a+b", Some(b"a b")),
    (b"a%20b", Some(b"a b")),
    (b"a%2Bb", Some(b"a+b")),
    (b"%41%42", Some(b"AB")),
    (b"%", None),
    (b"%4", None),
    (b"%zz", None),
    (b"%4g", None),
    (b"%e4%bd%a0", Some(b"\xe4\xbd\xa0")),
    (b"%ff", Some(b"\xff")),
    (b"%00", Some(b"\x00")),
    (b"a%2", None),
    (b"%%41", None),
    (b"%25", Some(b"%")),
    (b"%2f", Some(b"/")),
    (b"%3D%3d", Some(b"==")),
    (b"%7e%7E", Some(b"~~")),
    (b"a\xffb", Some(b"a\xffb")),
];

// Go: url.Parse("/" + s) then Path without the leading "/"
const UNESCAPE_PATH: &[(&[u8], Option<&[u8]>)] = &[
    (b"abc", Some(b"abc")),
    (b"a%20b", Some(b"a b")),
    (b"a+b", Some(b"a+b")),
    (b"a%2Fb", Some(b"a/b")),
    (b"%zz", None),
    (b"%", None),
    (b"%e4%bd%a0", Some(b"\xe4\xbd\xa0")),
    (b"%ff", Some(b"\xff")),
    (b"%00", Some(b"\x00")),
    (b"a b", Some(b"a b")),
    (b"a\xffb", Some(b"a\xffb")),
    (b"%25", Some(b"%")),
];

// Go: url.Parse("http://" + s + "/") then Host
const UNESCAPE_HOST: &[(&[u8], Option<&[u8]>)] = &[
    (b"example.com", Some(b"example.com")),
    (b"ex%c3%a4", Some(b"ex\xc3\xa4")),
    (b"ex%25", Some(b"ex%")),
    (b"ex%41", None),
    (b"ex%2f", None),
    (b"a b", None),
    (b"a+b", Some(b"a+b")),
    (b"a%", None),
    (b"\xc3\xa9", Some(b"\xc3\xa9")),
    (b"a_b", Some(b"a_b")),
    (b"a!b$c", Some(b"a!b$c")),
    (b"a<b>", Some(b"a<b>")),
    (b"a\"b", Some(b"a\"b")),
    (b"%7f", None),
    (b"%80", Some(b"\x80")),
    (b"%00", None),
    (b"A%2d", None),
    (b"a~b", Some(b"a~b")),
    (b"a%zz", None),
    (b"a{b}", None),
];

// Go: url.Parse("http://[::1%25" + s + "]/") then Host without "[::1%" and "]"
const UNESCAPE_ZONE: &[(&[u8], Option<&[u8]>)] = &[
    (b"en0", Some(b"en0")),
    (b"a%20b", Some(b"a b")),
    (b"a%2fb", None),
    (b"a%41b", Some(b"aAb")),
    (b"a%25b", Some(b"a%b")),
    (b"a b", None),
    (b"a_b", Some(b"a_b")),
    (b"a~b", Some(b"a~b")),
    (b"%7f", None),
    (b"%80", None),
    (b"a%zz", None),
    (b"a<b>", Some(b"a<b>")),
    (b"a\"b", Some(b"a\"b")),
    (b"a{b}", None),
    (b"%00", None),
    (b"%3a", Some(b":")),
];

// Go: url.Parse("http://" + s + "@h/") then User.Username()
const UNESCAPE_USER_PASSWORD: &[(&[u8], Option<&[u8]>)] = &[
    (b"user", Some(b"user")),
    (b"us%40er", Some(b"us@er")),
    (b"us%3aer", Some(b"us:er")),
    (b"%41%42", Some(b"AB")),
    (b"us%zz", None),
    (b"us%", None),
    (b"%ff", Some(b"\xff")),
    (b"us+er", Some(b"us+er")),
    (b"u~-_.!", Some(b"u~-_.!")),
    (b"%e4%bd%a0", Some(b"\xe4\xbd\xa0")),
    (b"%00", Some(b"\x00")),
    (b"u%25", Some(b"u%")),
];

// Go: url.Parse("http://h/#" + s) then Fragment
const UNESCAPE_FRAGMENT: &[(&[u8], Option<&[u8]>)] = &[
    (b"abc", Some(b"abc")),
    (b"a%20b", Some(b"a b")),
    (b"a+b", Some(b"a+b")),
    (b"a%2Fb", Some(b"a/b")),
    (b"%zz", None),
    (b"%", None),
    (b"%e4%bd%a0", Some(b"\xe4\xbd\xa0")),
    (b"%ff", Some(b"\xff")),
    (b"%00", Some(b"\x00")),
    (b"a b", Some(b"a b")),
    (b"a\xffb", Some(b"a\xffb")),
    (b"a?b/c", Some(b"a?b/c")),
];
