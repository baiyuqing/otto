//! Credential ranges in URL-style connections and Go MySQL DSNs.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use super::heuristics::{placeholder, sensitive_key};

static URLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b[a-z][a-z0-9+.-]{1,20}://[^\s<>"'`]+"#).expect("connection URL pattern")
});
static MYSQL_DSNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[^\s'"/:@]+:([^\s'"@]+)@(?:tcp|unix)\("#).expect("Go MySQL DSN pattern")
});

pub(super) fn finding_ranges(text: &str) -> Vec<Range<usize>> {
    let mut findings = Vec::new();
    for matched in URLS.find_iter(text) {
        let raw = matched.as_str().trim_end_matches([',', ';', ')', '}']);
        // Use the URL parser to validate the shape. Work on the source bytes
        // afterwards so percent encoding and useful host/database text survive.
        if url::Url::parse(raw).is_err() {
            continue;
        }
        let authority_start = raw.find("://").expect("URL scheme") + 3;
        let authority_end = raw[authority_start..]
            .find(['/', '?', '#'])
            .map_or(raw.len(), |offset| authority_start + offset);
        if let Some(at) = raw[authority_start..authority_end].rfind('@') {
            let userinfo_end = authority_start + at;
            if let Some(colon) = raw[authority_start..userinfo_end].find(':') {
                let password_start = authority_start + colon + 1;
                if password_start < userinfo_end && !placeholder(&raw[password_start..userinfo_end])
                {
                    findings.push(matched.start() + password_start..matched.start() + userinfo_end);
                }
            }
        }
        let Some(query_start) = raw.find('?') else {
            continue;
        };
        let query_end = raw[query_start..]
            .find('#')
            .map_or(raw.len(), |offset| query_start + offset);
        let mut cursor = query_start + 1;
        for pair in raw[cursor..query_end].split(['&', ';']) {
            if let Some((key, value)) = pair.split_once('=') {
                let decoded = url::form_urlencoded::parse(format!("{key}=").as_bytes())
                    .next()
                    .map(|(key, _)| key.into_owned())
                    .unwrap_or_default();
                if (sensitive_key(&decoded)
                    || matches!(decoded.to_ascii_lowercase().as_str(), "sig" | "signature"))
                    && !value.is_empty()
                    && !placeholder(value)
                {
                    let start = matched.start() + cursor + key.len() + 1;
                    findings.push(start..start + value.len());
                }
            }
            cursor += pair.len() + 1;
        }
    }
    for captures in MYSQL_DSNS.captures_iter(text) {
        let password = captures.get(1).expect("DSN password");
        if !placeholder(password.as_str()) {
            findings.push(password.range());
        }
    }
    findings
}
