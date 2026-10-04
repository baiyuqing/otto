//! The `web_search` backend: one HTTPS JSON search API.
//!
//! The backend is Brave Search. The key comes from the environment and is
//! sent only in a request header; it never appears in results, errors or
//! approval details.

use serde::Deserialize;
use url::Url;

/// The environment variable holding the Brave Search API key.
pub const KEY_ENV: &str = "OTTO_SEARCH_API_KEY";

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

/// One configured search backend.
#[derive(Clone)]
pub(super) struct Backend {
    endpoint: Url,
    key: String,
}

impl Backend {
    /// The default backend for `key`, or `None` when the key is empty.
    pub(super) fn from_key(key: &str) -> Option<Self> {
        let key = key.trim();
        (!key.is_empty()).then(|| Self {
            endpoint: Url::parse(ENDPOINT).expect("the endpoint constant is a valid URL"),
            key: key.to_owned(),
        })
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(endpoint: &str, key: &str) -> Self {
        Self {
            endpoint: Url::parse(endpoint).expect("test endpoint is valid"),
            key: key.to_owned(),
        }
    }

    pub(super) fn host(&self) -> String {
        self.endpoint
            .host_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    /// The request URL for `query` returning at most `count` results.
    pub(super) fn request_url(&self, query: &str, count: usize) -> Url {
        let mut url = self.endpoint.clone();
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("count", &count.to_string());
        url
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Deserialize)]
struct Envelope {
    web: Option<WebSection>,
}

#[derive(Deserialize)]
struct WebSection {
    #[serde(default)]
    results: Vec<Raw>,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

/// Maps a response body to hits, dropping entries without a public-looking
/// http(s) URL.
pub(super) fn parse_hits(body: &[u8]) -> Result<Vec<Hit>, String> {
    let envelope: Envelope =
        serde_json::from_slice(body).map_err(|_| "unexpected search response".to_string())?;
    Ok(envelope
        .web
        .map(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .filter(|raw| raw.url.starts_with("http://") || raw.url.starts_with("https://"))
        .map(|raw| Hit {
            title: plain(&raw.title),
            url: raw.url,
            snippet: plain(&raw.description),
        })
        .collect())
}

/// Removes inline markup (`<strong>`) and decodes the common entities.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn format_hits(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("No results for {query:?}.");
    }
    let mut out = format!("Search results for {query:?}:\n");
    for (index, hit) in hits.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", index + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_key_means_no_backend() {
        assert!(Backend::from_key("").is_none());
        assert!(Backend::from_key("  \n").is_none());
        let backend = Backend::from_key(" k ").expect("backend");
        assert_eq!(backend.key(), "k");
        assert_eq!(backend.host(), "api.search.brave.com");
    }

    #[test]
    fn the_request_url_encodes_the_query() {
        let backend = Backend::from_key("k").unwrap();
        let url = backend.request_url("rust & go?", 5);
        assert_eq!(url.host_str(), Some("api.search.brave.com"));
        let pairs: Vec<_> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            vec![("q".into(), "rust & go?".into()), ("count".into(), "5".into())]
        );
        assert!(!url.as_str().contains("k="), "the key is a header, not a parameter");
    }

    #[test]
    fn maps_a_recorded_response_and_strips_markup() {
        let body = br#"{"type":"search","web":{"results":[
          {"title":"The <strong>Rust</strong> Book","url":"https://doc.rust-lang.org/book/","description":"Learn &amp; <strong>build</strong>\n  things"},
          {"title":"bad","url":"javascript:alert(1)","description":"x"},
          {"title":"No snippet","url":"http://example.com/a"}
        ]}}"#;
        let hits = parse_hits(body).unwrap();
        assert_eq!(
            hits,
            vec![
                Hit {
                    title: "The Rust Book".into(),
                    url: "https://doc.rust-lang.org/book/".into(),
                    snippet: "Learn & build things".into()
                },
                Hit {
                    title: "No snippet".into(),
                    url: "http://example.com/a".into(),
                    snippet: String::new()
                },
            ]
        );
        let text = format_hits("rust", &hits);
        assert!(text.contains("1. The Rust Book\n   https://doc.rust-lang.org/book/"), "{text}");
        assert!(text.contains("2. No snippet"), "{text}");
    }

    #[test]
    fn no_results_and_bad_bodies() {
        assert_eq!(parse_hits(br#"{"type":"search"}"#).unwrap(), vec![]);
        assert_eq!(format_hits("q", &[]), "No results for \"q\".");
        assert!(parse_hits(b"<html>").is_err());
    }
}
