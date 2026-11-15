//! Parse vectors from Go's `url.Parse`; see the header of `gourl_tests.rs`.

use super::show;
use crate::gourl::{Url, parse};

struct User {
    username: &'static [u8],
    password: Option<&'static [u8]>,
    encoded: &'static [u8],
}

struct Want {
    scheme: &'static [u8],
    opaque: &'static [u8],
    user: Option<User>,
    host: &'static [u8],
    path: &'static [u8],
    raw_query: &'static [u8],
    fragment: &'static [u8],
}

/// One vector: the input and `None` when Go returned an error.
struct Case {
    input: &'static [u8],
    want: Option<Want>,
}

fn usr(username: &'static [u8], password: Option<&'static [u8]>, encoded: &'static [u8]) -> User {
    User {
        username,
        password,
        encoded,
    }
}

fn err(input: &'static [u8]) -> Case {
    Case { input, want: None }
}

#[allow(clippy::too_many_arguments)]
fn ok(
    input: &'static [u8],
    scheme: &'static [u8],
    opaque: &'static [u8],
    user: Option<User>,
    host: &'static [u8],
    path: &'static [u8],
    raw_query: &'static [u8],
    fragment: &'static [u8],
) -> Case {
    let want = Want {
        scheme,
        opaque,
        user,
        host,
        path,
        raw_query,
        fragment,
    };
    Case {
        input,
        want: Some(want),
    }
}

/// Describes a parse result with the same fields the Go rows hold.
fn describe(result: &Result<Url, ()>) -> String {
    let Ok(url) = result else {
        return "Err".to_string();
    };
    let user = url.user.as_ref().map(|u| {
        format!(
            "({}, {:?}, {})",
            show(u.username()),
            u.password().map(show),
            show(&u.encoded())
        )
    });
    format!(
        "scheme={} opaque={} user={user:?} host={} path={} query={} fragment={}",
        show(&url.scheme),
        show(&url.opaque),
        show(&url.host),
        show(&url.path),
        show(&url.raw_query),
        show(&url.fragment),
    )
}

fn describe_want(want: &Option<Want>) -> String {
    let Some(w) = want else {
        return "Err".to_string();
    };
    let user = w.user.as_ref().map(|u| {
        format!(
            "({}, {:?}, {})",
            show(u.username),
            u.password.map(show),
            show(u.encoded)
        )
    });
    format!(
        "scheme={} opaque={} user={user:?} host={} path={} query={} fragment={}",
        show(w.scheme),
        show(w.opaque),
        show(w.host),
        show(w.path),
        show(w.raw_query),
        show(w.fragment),
    )
}

/// Compares by rendered description, which covers every stored field.
fn mismatches(cases: &[Case]) -> Vec<String> {
    let mut out = Vec::new();
    for case in cases {
        let got = describe(&parse(case.input));
        let want = describe_want(&case.want);
        if got != want {
            out.push(format!(
                "{}\n  rust: {got}\n  go:   {want}",
                show(case.input)
            ));
        }
    }
    out
}

#[test]
fn parse_matches_go() {
    let failures = mismatches(&parse_cases());
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[rustfmt::skip]
fn parse_cases() -> Vec<Case> {
    vec![
        ok(b"http://example.com", b"http", b"", None, b"example.com", b"", b"", b""),
        ok(b"https://user:pass@proxy.example.com:8080/path?q=1#frag", b"https", b"", Some(usr(b"user", Some(b"pass"), b"user:pass")), b"proxy.example.com:8080", b"/path", b"q=1", b"frag"),
        ok(b"socks5://127.0.0.1:1080", b"socks5", b"", None, b"127.0.0.1:1080", b"", b"", b""),
        ok(b"http://user@host", b"http", b"", Some(usr(b"user", None, b"user")), b"host", b"", b"", b""),
        ok(b"http://user:@host", b"http", b"", Some(usr(b"user", Some(b""), b"user:")), b"host", b"", b"", b""),
        ok(b"http://:pass@host", b"http", b"", Some(usr(b"", Some(b"pass"), b":pass")), b"host", b"", b"", b""),
        ok(b"http://@host", b"http", b"", Some(usr(b"", None, b"")), b"host", b"", b"", b""),
        ok(b"http://:@host", b"http", b"", Some(usr(b"", Some(b""), b":")), b"host", b"", b"", b""),
        ok(b"http://user:p@ss@host", b"http", b"", Some(usr(b"user", Some(b"p@ss"), b"user:p%40ss")), b"host", b"", b"", b""),
        ok(b"http://u%40ser:p%3Ass@host", b"http", b"", Some(usr(b"u@ser", Some(b"p:ss"), b"u%40ser:p%3Ass")), b"host", b"", b"", b""),
        err(b"http://us%er@host"),
        err(b"http://user:p%zz@host"),
        ok(b"http://%41%42:%43@host", b"http", b"", Some(usr(b"AB", Some(b"C"), b"AB:C")), b"host", b"", b"", b""),
        ok(b"http://user:pa+ss@host", b"http", b"", Some(usr(b"user", Some(b"pa+ss"), b"user:pa+ss")), b"host", b"", b"", b""),
        err(b"http://us er@host"),
        err(b"http://user:pa ss@host"),
        err(b"http://\xc3\xbc:p@host"),
        ok(b"http://user:%ff%fe@host", b"http", b"", Some(usr(b"user", Some(b"\xff\xfe"), b"user:%FF%FE")), b"host", b"", b"", b""),
        ok(b"http://%ff@host", b"http", b"", Some(usr(b"\xff", None, b"%FF")), b"host", b"", b"", b""),
        ok(b"http://user:pass@", b"http", b"", Some(usr(b"user", Some(b"pass"), b"user:pass")), b"", b"", b"", b""),
        ok(b"//user:pass@host/path", b"", b"", Some(usr(b"user", Some(b"pass"), b"user:pass")), b"host", b"/path", b"", b""),
        ok(b"//host", b"", b"", None, b"host", b"", b"", b""),
        ok(b"///path", b"", b"", None, b"", b"///path", b"", b""),
        ok(b"////path", b"", b"", None, b"", b"////path", b"", b""),
        ok(b"host:8080", b"host", b"8080", None, b"", b"", b"", b""),
        ok(b"host.example.com", b"", b"", None, b"", b"host.example.com", b"", b""),
        ok(b"user:pass@host:8080", b"user", b"pass@host:8080", None, b"", b"", b"", b""),
        err(b"1host:80"),
        err(b":80"),
        ok(b"http:", b"http", b"", None, b"", b"", b"", b""),
        ok(b"http:foo", b"http", b"foo", None, b"", b"", b"", b""),
        ok(b"mailto:a@b.c", b"mailto", b"a@b.c", None, b"", b"", b"", b""),
        ok(b"HTTP://EXAMPLE.com", b"http", b"", None, b"EXAMPLE.com", b"", b"", b""),
        err(b"ht tp://x"),
        err(b"http://host:port"),
        err(b"http://host:80:90"),
        ok(b"foo://host:80:90", b"foo", b"", None, b"host:80:90", b"", b"", b""),
        ok(b"foo://a:80,b:90", b"foo", b"", None, b"a:80,b:90", b"", b"", b""),
        ok(b"http://host:", b"http", b"", None, b"host:", b"", b"", b""),
        err(b"http://host:+80"),
        ok(b"http://[::1]", b"http", b"", None, b"[::1]", b"", b"", b""),
        ok(b"http://[::1]:8080", b"http", b"", None, b"[::1]:8080", b"", b"", b""),
        ok(b"http://[::1%25en0]:80", b"http", b"", None, b"[::1%en0]:80", b"", b"", b""),
        err(b"http://[fe80::1%en0]"),
        err(b"http://[fe80::1%25]"),
        err(b"http://[::1"),
        err(b"http://::1]"),
        err(b"http://[::1]x"),
        ok(b"http://[::ffff:1.2.3.4]", b"http", b"", None, b"[::ffff:1.2.3.4]", b"", b"", b""),
        err(b"http://[1.2.3.4]"),
        err(b"http://[invalid]"),
        ok(b"http://[::1]:", b"http", b"", None, b"[::1]:", b"", b"", b""),
        err(b"http://a[::1]"),
        ok(b"http://[::1%25a%20b]", b"http", b"", None, b"[::1%a b]", b"", b"", b""),
        err(b"http://[::1%25a/b]"),
        ok(b"http://[fe80::1%25%65n0]", b"http", b"", None, b"[fe80::1%en0]", b"", b"", b""),
        err(b"http://[::1%25en0%00]"),
        err(b"http://ex%41mple.com"),
        ok(b"http://ex%c3%a4mple.com", b"http", b"", None, b"ex\xc3\xa4mple.com", b"", b"", b""),
        ok(b"http://ex%25mple.com", b"http", b"", None, b"ex%mple.com", b"", b"", b""),
        err(b"http://ex%2Fmple.com"),
        err(b"http://exa mple.com"),
        ok(b"http://exa_mple.com", b"http", b"", None, b"exa_mple.com", b"", b"", b""),
        ok(b"http://exa<mple>.com", b"http", b"", None, b"exa<mple>.com", b"", b"", b""),
        err(b"http://host\\path"),
        ok(b"http:\\\\host", b"http", b"\\\\host", None, b"", b"", b"", b""),
        err(b"http://user\\@host"),
        err(b"http://host\x00"),
        err(b"http://host\x7f"),
        err(b"http://host\x0a/"),
        err(b"http://host/pa\x09th"),
        err(b"http://host/%zz"),
        err(b"http://host/%"),
        err(b"http://host/%4"),
        ok(b"http://host/a%2Fb", b"http", b"", None, b"host", b"/a/b", b"", b""),
        ok(b"http://host/a b", b"http", b"", None, b"host", b"/a b", b"", b""),
        ok(b"http://host/?q=%zz", b"http", b"", None, b"host", b"/", b"q=%zz", b""),
        err(b"http://host/#%zz"),
        ok(b"http://host/#a%20b", b"http", b"", None, b"host", b"/", b"", b"a b"),
        ok(b"http://host/#a#b", b"http", b"", None, b"host", b"/", b"", b"a#b"),
        ok(b"http://host?", b"http", b"", None, b"host", b"", b"", b""),
        ok(b"http://host?a?b", b"http", b"", None, b"host", b"", b"a?b", b""),
        ok(b"http://host/p?q#f", b"http", b"", None, b"host", b"/p", b"q", b"f"),
        ok(b"http://host/\xff", b"http", b"", None, b"host", b"/\xff", b"", b""),
        ok(b"http://host/%ff", b"http", b"", None, b"host", b"/\xff", b"", b""),
        ok(b"http://h\xc3\xa9st/", b"http", b"", None, b"h\xc3\xa9st", b"/", b"", b""),
        ok(b"*", b"", b"", None, b"", b"*", b"", b""),
        ok(b"", b"", b"", None, b"", b"", b"", b""),
        ok(b"#", b"", b"", None, b"", b"", b"", b""),
        ok(b"?", b"", b"", None, b"", b"", b"", b""),
        ok(b"/", b"", b"", None, b"", b"/", b"", b""),
        ok(b"//", b"", b"", None, b"", b"", b"", b""),
        ok(b"///", b"", b"", None, b"", b"///", b"", b""),
        ok(b"a/b:c", b"", b"", None, b"", b"a/b:c", b"", b""),
        ok(b"./a:b", b"", b"", None, b"", b"./a:b", b"", b""),
        ok(b"http://user:pass@host:8080?x@y", b"http", b"", Some(usr(b"user", Some(b"pass"), b"user:pass")), b"host:8080", b"", b"x@y", b""),
        ok(b"http://host#user:pass@x", b"http", b"", None, b"host", b"", b"", b"user:pass@x"),
        ok(b"http://us@er:pa@ss@host", b"http", b"", Some(usr(b"us@er", Some(b"pa@ss"), b"us%40er:pa%40ss")), b"host", b"", b"", b""),
        ok(b"http://user:pa:ss@host", b"http", b"", Some(usr(b"user", Some(b"pa:ss"), b"user:pa%3Ass")), b"host", b"", b"", b""),
        ok(b"http://u%3Aser@host", b"http", b"", Some(usr(b"u:ser", None, b"u%3Aser")), b"host", b"", b"", b""),
        ok(b"http://%3a@host", b"http", b"", Some(usr(b":", None, b"%3A")), b"host", b"", b"", b""),
        err(b"http://user%@host"),
        ok(b"http://user:pass@@host", b"http", b"", Some(usr(b"user", Some(b"pass@"), b"user:pass%40")), b"host", b"", b"", b""),
        ok(b"http://:@:", b"http", b"", Some(usr(b"", Some(b""), b":")), b":", b"", b"", b""),
        ok(b"http://@@", b"http", b"", Some(usr(b"@", None, b"%40")), b"", b"", b"", b""),
        ok(b"http://user:p%25ss@host", b"http", b"", Some(usr(b"user", Some(b"p%ss"), b"user:p%25ss")), b"host", b"", b"", b""),
        ok(b"http://user:p~-_.!$&'()*+,;=@host", b"http", b"", Some(usr(b"user", Some(b"p~-_.!$&'()*+,;="), b"user:p~-_.%21$&%27%28%29%2A+,;=")), b"host", b"", b"", b""),
        err(b"http://user:p#ss@host"),
        err(b"http://user:p?ss@host"),
        err(b"http://user:p/ss@host"),
        ok(b"HTTP://U:P@H", b"http", b"", Some(usr(b"U", Some(b"P"), b"U:P")), b"H", b"", b"", b""),
        ok(b"socks5h://u:p@h:1", b"socks5h", b"", Some(usr(b"u", Some(b"p"), b"u:p")), b"h:1", b"", b"", b""),
        ok(b"ftp://:@h", b"ftp", b"", Some(usr(b"", Some(b""), b":")), b"h", b"", b"", b""),
        ok(b"x-y.z+w://h", b"x-y.z+w", b"", None, b"h", b"", b"", b""),
        err(b"+x://h"),
        ok(b"http://h/%2e%2e/", b"http", b"", None, b"h", b"/../", b"", b""),
        ok(b"http://[::1]:80/%25", b"http", b"", None, b"[::1]:80", b"/%", b"", b""),
        err(b"http://[%3a%3a1]"),
        err(b"http://[::1]%25"),
        err(b"http://[v1.fe]"),
        ok(b"http://[::ffff:1.2.3.4%25z]", b"http", b"", None, b"[::ffff:1.2.3.4%z]", b"", b"", b""),
        err(b"http://[1.2.3.4%25z]"),
        err(b"http://[::1%25en0]:80:90"),
        err(b"http://[::1][::2]"),
        err(b"http://[[::1]]"),
        err(b"http://%5b::1%5d/"),
        ok(b"http://user:pass@[::1]:80", b"http", b"", Some(usr(b"user", Some(b"pass"), b"user:pass")), b"[::1]:80", b"", b"", b""),
        ok(b"HtTp://h", b"http", b"", None, b"h", b"", b"", b""),
        ok(b"http://h:0080", b"http", b"", None, b"h:0080", b"", b"", b""),
        ok(b"http://h:99999999999", b"http", b"", None, b"h:99999999999", b"", b"", b""),
        ok(b"http://a,b/", b"http", b"", None, b"a,b", b"/", b"", b""),
        ok(b"http://a;b:c@d", b"http", b"", Some(usr(b"a;b", Some(b"c"), b"a;b:c")), b"d", b"", b"", b""),
        err(b"http://\xe4\xbd\xa0:p@h"),
        ok(b"http://[::1%25en0%25x]", b"http", b"", None, b"[::1%en0%x]", b"", b"", b""),
        ok(b"http://[0:0:0:0:0:0:0:1]", b"http", b"", None, b"[0:0:0:0:0:0:0:1]", b"", b"", b""),
        ok(b"http://[::1.2.3.4]", b"http", b"", None, b"[::1.2.3.4]", b"", b"", b""),
        err(b"http://[1::2::3]"),
        err(b"http://[::1%25 ]"),
        ok(b"http://[fe80::1%25en%30]", b"http", b"", None, b"[fe80::1%en0]", b"", b"", b""),
        ok(b"http://[2001:db8::1]:443/x", b"http", b"", None, b"[2001:db8::1]:443", b"/x", b"", b""),
        err(b"http://[0::1:]"),
        err(b"http://[::g]"),
        err(b"http://[1:2:3:4:5:6:7:8:9]"),
        err(b"http://[::1]:abc"),
        err(b"http://[::ffff:01.2.3.4]"),
        ok(b"http://[::1%25a%25b]", b"http", b"", None, b"[::1%a%b]", b"", b"", b""),
        ok(b"http://[::1%25a<b>]", b"http", b"", None, b"[::1%a<b>]", b"", b"", b""),
        err(b"http://[::1%25a%7fb]"),
        err(b"http://[::1%25a%2fb]"),
        err(b"http://[]"),
        err(b"http://[]:80"),
        ok(b"http://[::1]:80/[x]", b"http", b"", None, b"[::1]:80", b"/[x]", b"", b""),
        ok(b"http://h/[x]", b"http", b"", None, b"h", b"/[x]", b"", b""),
        ok(b"http://h]/", b"http", b"", None, b"h]", b"/", b"", b""),
        err(b"http://h[/"),
        err(b"http://[::1]@h"),
        ok(b"http://u:p@h:80@x", b"http", b"", Some(usr(b"u", Some(b"p@h:80"), b"u:p%40h%3A80")), b"x", b"", b"", b""),
        ok(b"http::", b"http", b":", None, b"", b"", b"", b""),
        ok(b"a:b:c", b"a", b"b:c", None, b"", b"", b"", b""),
        ok(b"mailto:%zz", b"mailto", b"%zz", None, b"", b"", b"", b""),
        ok(b"http:%", b"http", b"%", None, b"", b"", b"", b""),
        ok(b"//host:80:90", b"", b"", None, b"host:80:90", b"", b"", b""),
        err(b"//[::1]:80:90"),
        ok(b"foo://host:80:90/path:x", b"foo", b"", None, b"host:80:90", b"/path:x", b"", b""),
        err(b"https://h:443:80"),
        ok(b"x://h:1,h2:2", b"x", b"", None, b"h:1,h2:2", b"", b"", b""),
        ok(b"x://h:a,h2:2", b"x", b"", None, b"h:a,h2:2", b"", b"", b""),
        ok(b"x://:", b"x", b"", None, b":", b"", b"", b""),
        err(b"http://h?\x7f"),
        ok(b"x://h/\x80", b"x", b"", None, b"h", b"/\x80", b"", b""),
        err(b"http://h/\x00"),
        ok(b"http://h/ %41", b"http", b"", None, b"h", b"/ A", b"", b""),
        ok(b"http://[::1%25en0]?x#y", b"http", b"", None, b"[::1%en0]", b"", b"x", b"y"),
        err(b"http://u:p%2@h"),
        err(b"http://u%2:p@h"),
        err(b"http://u:p@h%2"),
        err(b"http://h%2"),
        ok(b"http://h%e4%bd%a0:80", b"http", b"", None, b"h\xe4\xbd\xa0:80", b"", b"", b""),
        ok(b"http://%e4%bd%a0.example/", b"http", b"", None, b"\xe4\xbd\xa0.example", b"/", b"", b""),
        ok(b"http://%C3%A4/", b"http", b"", None, b"\xc3\xa4", b"/", b"", b""),
        err(b"http://h:80%25/"),
        err(b"http://[::1]:80%25/"),
        ok(b"http://u:%e4%bd@h", b"http", b"", Some(usr(b"u", Some(b"\xe4\xbd"), b"u:%E4%BD")), b"h", b"", b"", b""),
        err(b"http://u:\"@h"),
        err(b"http://u:<>@h"),
        err(b"http://u:[]@h"),
        err(b"http://u:{}@h"),
        err(b"http://u:|@h"),
        err(b"http://u:^@h"),
        err(b"http://u:`@h"),
        err(b"http://u:\\@h"),
        err(b"http://u:\x7f@h"),
        ok(b"http://[::1%25\xc3\xa9]", b"http", b"", None, b"[::1%\xc3\xa9]", b"", b"", b""),
        err(b"http://[::\xff]"),
    ]
}

/// Rows where the Rust parser differs from Go. Each row holds the Go result;
/// `known_differences_keep_current_rust_result` asserts what Rust returns now.
///
/// Both inputs put bytes that are not valid UTF-8 inside the zone of a
/// bracketed IPv6 host. Go's `netip.ParseAddr` accepts any bytes after `%`;
/// `parse_ip_literal` in `gourl.rs` calls `std::str::from_utf8` first and
/// returns an error. `urlprivacy::userinfo_forms` takes `&[u8]`, but its
/// callers (`runtime/boundary.rs`, `sandbox/environment.rs`,
/// `tool/bash/review.rs`) pass `str::as_bytes()`, which is valid UTF-8, and a
/// percent-escaped zone byte at or above 0x80 is rejected by both Go and Rust
/// in `unescape(Zone)`; so the difference is not reachable from them.
#[rustfmt::skip]
fn known_differences() -> Vec<Case> {
    vec![
        ok(b"http://[::1%25\xff]", b"http", b"", None, b"[::1%\xff]", b"", b"", b""),
        ok(b"http://[::1%25a\xc3]", b"http", b"", None, b"[::1%a\xc3]", b"", b"", b""),
    ]
}

#[test]
fn known_differences_keep_current_rust_result() {
    for case in known_differences() {
        let rust = parse(case.input);
        assert!(
            rust.is_err(),
            "Rust now accepts {}; move the row to the main table",
            show(case.input)
        );
        assert!(
            case.want.is_some(),
            "Go returned an error for {}",
            show(case.input)
        );
    }
}
