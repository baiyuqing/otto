//! Tests for proxy userinfo extraction.

use super::*;

#[test]
fn proxy_userinfo_is_collected_without_rewriting_the_proxy() {
    let proxy = "http://raw%20user:raw%2Fpass@[2001:db8::1]:8443/path";
    let second = "socks5://solo%20user@[::1]:9000";
    let network_path = "//network%20user:network%2Fpass@[2001:db8::2]:9443/path@ignored";
    let snapshot = resolve_environment(&options(&[
        &format!("HTTPS_PROXY={proxy}"),
        &format!("all_proxy={second}"),
        &format!("http_proxy={network_path}"),
    ]))
    .unwrap();
    let environment = snapshot_environment(&snapshot);
    assert_eq!(environment["HTTPS_PROXY"], proxy);
    assert_eq!(environment["all_proxy"], second);
    assert_eq!(environment["http_proxy"], network_path);
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "raw%20user:raw%2Fpass",
            "raw user:raw/pass",
            "raw user",
            "raw/pass",
            "solo%20user",
            "solo user",
            "network%20user:network%2Fpass",
            "network user:network/pass",
        ],
    );
    assert_redactions_sorted(snapshot.redaction_values());
}

#[test]
fn malformed_proxy_authorities_are_extracted_conservatively() {
    let cases: &[(&str, &[&str])] = &[
        (
            "http:///proxy-user:proxy-pass@example.test",
            &["proxy-user:proxy-pass", "proxy-user", "proxy-pass"],
        ),
        (
            r"http:\\proxy%20user:proxy%2Fpass@example.test\path",
            &[
                "proxy%20user:proxy%2Fpass",
                "proxy%20user",
                "proxy%2Fpass",
                "proxy user:proxy/pass",
                "proxy user",
                "proxy/pass",
            ],
        ),
        (
            "proxy%20user:proxy%2Fpass@example.test/path",
            &[
                "proxy%20user:proxy%2Fpass",
                "proxy%20user",
                "proxy%2Fpass",
                "proxy user:proxy/pass",
                "proxy user",
                "proxy/pass",
            ],
        ),
        (
            "ht!tp://odd%20user:odd%2Fpass@example.test/path",
            &[
                "odd%20user:odd%2Fpass",
                "odd%20user",
                "odd%2Fpass",
                "odd user:odd/pass",
                "odd user",
                "odd/pass",
            ],
        ),
        (
            "http:odd%20user:odd%2Fpass@example.test/path",
            &[
                "http:odd%20user:odd%2Fpass",
                "odd%20user:odd%2Fpass",
                "odd%20user",
                "odd%2Fpass",
                "odd user",
                "odd/pass",
            ],
        ),
        (
            "http://[::1/path/proxy%20user:proxy%2Fpass@example.test",
            &[
                "proxy%20user:proxy%2Fpass",
                "proxy%20user",
                "proxy%2Fpass",
                "proxy user",
                "proxy/pass",
            ],
        ),
        (
            "http://bad%zz:pass%2Fword@[::1]:8080/path?next=@ignored#@ignored",
            &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
        ),
        (
            "http://user%20name:bad%zz@[2001:db8::1]:8080/path",
            &["user%20name:bad%zz", "user%20name", "bad%zz", "user name"],
        ),
        (
            "http://bad%zz:pass%2Fword@[::1]:8080/path/user:other@example.test",
            &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
        ),
        (
            "http:///real%20user:real%2Fpass@proxy/path%20user:path%2Fpass@example",
            &[
                "real%20user:real%2Fpass",
                "real user:real/pass",
                "path%20user:path%2Fpass",
                "path user:path/pass",
            ],
        ),
        (
            "///raw%20user:raw%2Fpass@host/path@ignored",
            &["raw%20user:raw%2Fpass", "raw user:raw/pass", "path"],
        ),
    ];
    for (proxy, want) in cases {
        let Err(rejection) = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")]))
        else {
            panic!("{proxy} should be rejected");
        };
        let snapshot = rejection.snapshot();
        assert!(snapshot.entries().is_none(), "{proxy}");
        assert!(!snapshot.redactions_complete(), "{proxy}");
        assert_redactions_contain(snapshot.redaction_values(), want);
    }
}

#[test]
fn percent_decoded_invalid_utf8_proxy_userinfo_is_canonicalized() {
    let proxy = "http://user%FF:pass%C0%AF@[::1]:8080/path";
    let snapshot = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap();
    assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], proxy);
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "user%FF:pass%C0%AF",
            "user%FF",
            "pass%C0%AF",
            "user\u{fffd}:pass\u{fffd}\u{fffd}",
            "user\u{fffd}",
            "pass\u{fffd}\u{fffd}",
        ],
    );
}

#[test]
fn a_valid_proxy_path_query_or_fragment_at_is_not_userinfo() {
    let proxy = "http://[2001:db8::1]:8080/path/user:pass@example.test?next=user:pass@example.test#user:pass@example.test";
    let snapshot = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap();
    assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], proxy);
    assert!(snapshot.redaction_values().is_empty());
}

#[test]
fn a_malformed_proxy_path_at_is_incomplete_without_inventing_userinfo() {
    let proxy = "http://[2001:db8::1]:8080/path/user:pass@example.test?broken=%zz";
    let rejection = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap_err();
    assert!(rejection.snapshot().entries().is_none());
    assert!(!rejection.snapshot().redactions_complete());
    assert!(rejection.snapshot().redaction_values().is_empty());
}

#[test]
fn proxy_values_without_a_literal_at_are_preserved() {
    for value in [
        "http://example.test/path%zz",
        "http:///example.test/path",
        r"http:\\example.test\path",
        "example.test:8080",
        "//example.test:8443",
        "http://:8080",
        "http://example.test:99999",
    ] {
        let Ok(snapshot) = resolve_environment(&options(&[&format!("HTTPS_PROXY={value}")])) else {
            panic!("{value} was rejected");
        };
        assert!(snapshot.redactions_complete(), "{value}");
        assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], value);
    }
}

#[test]
fn no_proxy_bypass_lists_are_preserved_case_insensitively() {
    let host = [
        "NO_PROXY=localhost,127.0.0.1,.example.test,[::1],*",
        "no_proxy=user:pass@example.test,service.local",
    ];
    let snapshot = resolve_environment(&options(&host)).unwrap();
    assert!(snapshot.redactions_complete());
    assert_eq!(snapshot.entries().unwrap(), host);
    assert!(snapshot.redaction_values().is_empty());
}
