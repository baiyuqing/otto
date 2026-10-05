//! The `web_search` backends.
//!
//! One backend is chosen when the tools are built, in this order:
//!
//! 1. Google Custom Search JSON API, when `GOOGLE_API_KEY` and
//!    `GOOGLE_SEARCH_ENGINE_ID` are both set;
//! 2. Brave Search API, when `BRAVE_API_KEY` is set;
//! 3. Bing's public results page, which needs no key.
//!
//! The choice is by configuration only. A runtime failure of the chosen
//! backend (quota, an outage, a changed page) is reported as an error; the
//! tool does not fall through to another host, because each host is approved
//! separately. API keys travel only in a request header or query parameter of
//! the request itself and never appear in results, errors or approvals.

use std::sync::OnceLock;

use base64::Engine;
use regex::Regex;
use serde::Deserialize;
use url::Url;

pub const GOOGLE_KEY_ENV: &str = "GOOGLE_API_KEY";
pub const GOOGLE_ENGINE_ENV: &str = "GOOGLE_SEARCH_ENGINE_ID";
pub const BRAVE_KEY_ENV: &str = "BRAVE_API_KEY";

const GOOGLE_ENDPOINT: &str = "https://www.googleapis.com/customsearch/v1";
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const BING_ENDPOINT: &str = "https://www.bing.com/search";

/// The most results the Google API returns per request.
const GOOGLE_MAX: usize = 10;

/// One configured search backend.
#[derive(Clone)]
pub(super) enum Backend {
    Google {
        endpoint: Url,
        key: String,
        engine: String,
    },
    Brave {
        endpoint: Url,
        key: String,
    },
    Bing {
        endpoint: Url,
    },
}

/// A request to send: the URL and extra headers.
pub(super) struct Request {
    pub url: Url,
    pub accept: &'static str,
    pub headers: Vec<(&'static str, String)>,
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

impl Backend {
    /// The backend for the environment `env` reads, by the priority above.
    pub(super) fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let url = |text: &str| Url::parse(text).expect("endpoint constant is a valid URL");
        if let (Some(key), Some(engine)) = (
            nonempty(env(GOOGLE_KEY_ENV)),
            nonempty(env(GOOGLE_ENGINE_ENV)),
        ) {
            return Self::Google {
                endpoint: url(GOOGLE_ENDPOINT),
                key,
                engine,
            };
        }
        if let Some(key) = nonempty(env(BRAVE_KEY_ENV)) {
            return Self::Brave {
                endpoint: url(BRAVE_ENDPOINT),
                key,
            };
        }
        Self::Bing {
            endpoint: url(BING_ENDPOINT),
        }
    }

    /// The secret values this backend sends, which queries must not contain.
    pub(super) fn secrets(&self) -> Vec<String> {
        match self {
            Self::Google { key, engine, .. } => vec![key.clone(), engine.clone()],
            Self::Brave { key, .. } => vec![key.clone()],
            Self::Bing { .. } => Vec::new(),
        }
    }

    /// Whether this is the backend that needs no key.
    pub(super) fn is_keyless(&self) -> bool {
        matches!(self, Self::Bing { .. })
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Google { .. } => "Google",
            Self::Brave { .. } => "Brave",
            Self::Bing { .. } => "Bing",
        }
    }

    fn endpoint(&self) -> &Url {
        match self {
            Self::Google { endpoint, .. }
            | Self::Brave { endpoint, .. }
            | Self::Bing { endpoint } => endpoint,
        }
    }

    /// The largest `count` the backend accepts.
    pub(super) fn max_count(&self, requested: usize) -> usize {
        match self {
            Self::Google { .. } => requested.min(GOOGLE_MAX),
            _ => requested,
        }
    }

    /// The request for `query` returning at most `count` results.
    pub(super) fn request(&self, query: &str, count: usize) -> Request {
        let mut url = self.endpoint().clone();
        let count = count.to_string();
        match self {
            Self::Google { key, engine, .. } => {
                url.query_pairs_mut()
                    .append_pair("key", key)
                    .append_pair("cx", engine)
                    .append_pair("q", query)
                    .append_pair("num", &count);
                Request {
                    url,
                    accept: "application/json",
                    headers: Vec::new(),
                }
            }
            Self::Brave { key, .. } => {
                url.query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("count", &count);
                Request {
                    url,
                    accept: "application/json",
                    headers: vec![("X-Subscription-Token", key.clone())],
                }
            }
            Self::Bing { .. } => {
                url.query_pairs_mut().append_pair("q", query);
                Request {
                    url,
                    accept: "text/html",
                    headers: Vec::new(),
                }
            }
        }
    }

    /// Maps a response body to at most `count` hits.
    pub(super) fn parse(&self, body: &[u8], count: usize) -> Result<Vec<Hit>, String> {
        let mut hits = match self {
            Self::Google { .. } => parse_google(body)?,
            Self::Brave { .. } => parse_brave(body)?,
            Self::Bing { .. } => parse_bing(&String::from_utf8_lossy(body))?,
        };
        hits.truncate(count);
        Ok(hits)
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(self, endpoint: &str) -> Self {
        let endpoint = Url::parse(endpoint).expect("test endpoint is valid");
        match self {
            Self::Google { key, engine, .. } => Self::Google {
                endpoint,
                key,
                engine,
            },
            Self::Brave { key, .. } => Self::Brave { endpoint, key },
            Self::Bing { .. } => Self::Bing { endpoint },
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn public_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

// ----- Google

#[derive(Deserialize)]
struct GoogleEnvelope {
    #[serde(default)]
    items: Vec<GoogleItem>,
}

#[derive(Deserialize)]
struct GoogleItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    snippet: String,
}

fn parse_google(body: &[u8]) -> Result<Vec<Hit>, String> {
    let envelope: GoogleEnvelope =
        serde_json::from_slice(body).map_err(|_| "unexpected search response".to_string())?;
    Ok(envelope
        .items
        .into_iter()
        .filter(|item| public_http(&item.link))
        .map(|item| Hit {
            title: plain(&item.title),
            url: item.link,
            snippet: plain(&item.snippet),
        })
        .collect())
}

// ----- Brave

#[derive(Deserialize)]
struct BraveEnvelope {
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveItem>,
}

#[derive(Deserialize)]
struct BraveItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

fn parse_brave(body: &[u8]) -> Result<Vec<Hit>, String> {
    let envelope: BraveEnvelope =
        serde_json::from_slice(body).map_err(|_| "unexpected search response".to_string())?;
    Ok(envelope
        .web
        .map(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .filter(|item| public_http(&item.url))
        .map(|item| Hit {
            title: plain(&item.title),
            url: item.url,
            snippet: plain(&item.description),
        })
        .collect())
}

// ----- Bing

fn bing_patterns() -> &'static (Regex, Regex, Regex) {
    static PATTERNS: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        (
            Regex::new(r#"(?s)<h2[^>]*>\s*<a\b[^>]*?\bhref="([^"]+)"[^>]*>(.*?)</a>"#)
                .expect("title pattern is valid"),
            Regex::new(r"(?s)<p\b[^>]*>(.*?)</p>").expect("snippet pattern is valid"),
            Regex::new(r"(?s)<link\b[^>]*>").expect("link pattern is valid"),
        )
    })
}

/// Parses Bing's results page.
///
/// This reads HTML that Bing does not document, so a layout change can break
/// it. A page with no recognizable result is an error unless Bing says there
/// were none, so a block or a redesign is not reported as "no results".
fn parse_bing(html: &str) -> Result<Vec<Hit>, String> {
    let (title, snippet, link) = bing_patterns();
    let mut hits = Vec::new();
    for chunk in html.split("<li class=\"b_algo\"").skip(1) {
        let chunk = chunk.split("</li>").next().unwrap_or(chunk);
        let chunk = link.replace_all(chunk, "");
        let Some(found) = title.captures(&chunk) else {
            continue;
        };
        let href = decode_entities(&found[1]);
        let url = bing_target(&href);
        if !public_http(&url) {
            continue;
        }
        let snippet = snippet
            .captures(&chunk)
            .map(|found| plain(&found[1]))
            .unwrap_or_default();
        hits.push(Hit {
            title: plain(&found[2]),
            url,
            snippet,
        });
    }
    if hits.is_empty() && !html.contains("b_no") {
        return Err(
            "the Bing results page had no recognizable results; Bing may be blocking \
             automated requests or may have changed its page. Set BRAVE_API_KEY, or \
             GOOGLE_API_KEY and GOOGLE_SEARCH_ENGINE_ID, to use an official search API"
                .to_string(),
        );
    }
    Ok(hits)
}

/// The real target of a Bing result link: `bing.com/ck/a?...&u=a1<base64url>`
/// carries it, other links are returned unchanged.
fn bing_target(href: &str) -> String {
    let Ok(url) = Url::parse(href) else {
        return href.to_owned();
    };
    if url.host_str() != Some("www.bing.com") || !url.path().starts_with("/ck/") {
        return href.to_owned();
    }
    let Some((_, encoded)) = url.query_pairs().find(|(name, _)| name == "u") else {
        return href.to_owned();
    };
    let Some(encoded) = encoded.strip_prefix("a1") else {
        return href.to_owned();
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|target| public_http(target))
        .unwrap_or_else(|| href.to_owned())
}

// ----- shared

/// Decodes the HTML entities result pages use.
fn decode_entities(text: &str) -> String {
    static ENTITY: OnceLock<Regex> = OnceLock::new();
    let entity = ENTITY.get_or_init(|| {
        Regex::new(r"&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);").expect("entity pattern is valid")
    });
    entity
        .replace_all(text, |found: &regex::Captures| {
            let name = &found[1];
            let decoded = if let Some(hex) = name.strip_prefix("#x") {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(decimal) = name.strip_prefix('#') {
                decimal.parse().ok().and_then(char::from_u32)
            } else {
                match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => None,
                }
            };
            decoded.map_or_else(|| found[0].to_owned(), |c| c.to_string())
        })
        .into_owned()
}

/// Removes inline markup and decodes entities.
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
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn format_hits(backend: &Backend, query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("No results for {query:?} ({}).", backend.label());
    }
    let mut out = format!("Search results for {query:?} ({}):\n", backend.label());
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
    use std::collections::HashMap;

    fn backend(vars: &[(&str, &str)]) -> Backend {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Backend::from_env(&|name| vars.get(name).cloned())
    }

    #[test]
    fn the_backend_follows_google_then_brave_then_bing() {
        assert_eq!(backend(&[]).label(), "Bing");
        assert_eq!(backend(&[("BRAVE_API_KEY", " ")]).label(), "Bing");
        assert_eq!(backend(&[("BRAVE_API_KEY", "b-key")]).label(), "Brave");
        // Google needs both variables.
        assert_eq!(backend(&[("GOOGLE_API_KEY", "g")]).label(), "Bing");
        assert_eq!(
            backend(&[("GOOGLE_API_KEY", "g"), ("BRAVE_API_KEY", "b")]).label(),
            "Brave"
        );
        assert_eq!(
            backend(&[("GOOGLE_SEARCH_ENGINE_ID", "cx"), ("BRAVE_API_KEY", "b")]).label(),
            "Brave"
        );
        let all = [
            ("GOOGLE_API_KEY", "g"),
            ("GOOGLE_SEARCH_ENGINE_ID", "cx"),
            ("BRAVE_API_KEY", "b"),
        ];
        assert_eq!(backend(&all).label(), "Google");
        assert_eq!(
            backend(&all).endpoint().host_str().unwrap(),
            "www.googleapis.com"
        );
        assert_eq!(backend(&[]).endpoint().host_str().unwrap(), "www.bing.com");
        assert_eq!(
            backend(&[("BRAVE_API_KEY", "b")])
                .endpoint()
                .host_str()
                .unwrap(),
            "api.search.brave.com"
        );
    }

    #[test]
    fn secrets_are_the_values_a_backend_sends() {
        assert!(backend(&[]).secrets().is_empty());
        assert_eq!(backend(&[("BRAVE_API_KEY", " k ")]).secrets(), vec!["k"]);
        let google = backend(&[("GOOGLE_API_KEY", "g"), ("GOOGLE_SEARCH_ENGINE_ID", "cx")]);
        assert_eq!(google.secrets(), vec!["g", "cx"]);
    }

    #[test]
    fn requests_encode_the_query_and_keep_keys_out_of_the_wrong_place() {
        let google = backend(&[("GOOGLE_API_KEY", "gk"), ("GOOGLE_SEARCH_ENGINE_ID", "cx1")]);
        let request = google.request("rust & go?", 5);
        let pairs: Vec<_> = request.url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            vec![
                ("key".into(), "gk".into()),
                ("cx".into(), "cx1".into()),
                ("q".into(), "rust & go?".into()),
                ("num".into(), "5".into()),
            ]
        );
        assert!(request.headers.is_empty());
        assert_eq!(google.max_count(20), 10);

        let brave = backend(&[("BRAVE_API_KEY", "bk")]);
        let request = brave.request("x", 7);
        assert!(!request.url.as_str().contains("bk"), "the key is a header");
        assert_eq!(
            request.headers,
            vec![("X-Subscription-Token", "bk".to_string())]
        );
        assert_eq!(brave.max_count(20), 20);

        let bing = backend(&[]);
        let request = bing.request("a b", 8);
        assert_eq!(request.url.as_str(), "https://www.bing.com/search?q=a+b");
        assert_eq!(request.accept, "text/html");
    }

    #[test]
    fn google_maps_a_recorded_response() {
        let body = br#"{"kind":"customsearch#search","items":[
          {"title":"The Rust Book","link":"https://doc.rust-lang.org/book/","snippet":"Learn &amp; build\nthings"},
          {"title":"bad","link":"javascript:alert(1)","snippet":"x"}]}"#;
        let hits = parse_google(body).unwrap();
        assert_eq!(
            hits,
            vec![Hit {
                title: "The Rust Book".into(),
                url: "https://doc.rust-lang.org/book/".into(),
                snippet: "Learn & build things".into()
            }]
        );
        assert_eq!(
            parse_google(br#"{"searchInformation":{}}"#).unwrap(),
            vec![]
        );
        assert!(parse_google(b"<html>").is_err());
    }

    #[test]
    fn brave_maps_a_recorded_response_and_strips_markup() {
        let body = br#"{"type":"search","web":{"results":[
          {"title":"The <strong>Rust</strong> Book","url":"https://doc.rust-lang.org/book/","description":"Learn &amp; <strong>build</strong>\n  things"},
          {"title":"bad","url":"javascript:alert(1)","description":"x"},
          {"title":"No snippet","url":"http://example.com/a"}]}}"#;
        let hits = parse_brave(body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "The Rust Book");
        assert_eq!(hits[0].snippet, "Learn & build things");
        assert_eq!(hits[1].snippet, "");
        assert_eq!(parse_brave(br#"{"type":"search"}"#).unwrap(), vec![]);
        assert!(parse_brave(b"<html>").is_err());
    }

    /// A trimmed copy of a real Bing results page: stylesheet `<link>` tags
    /// inside the item, a `ck/a` redirect link carrying the target as
    /// base64url, a dated snippet, and a result with a direct link.
    const BING_PAGE: &str = r#"<ol id="b_results"><li class="b_algo" data-id iid=SERP.5328><link rel="stylesheet" href="/rp/a.css" type="text/css"/><div class="b_tpcn"><a class="tilk" href="https://rust-lang.github.io/"><div class="tptxt">rust-lang.github.io</div></a></div><h2><a target="_blank" href="https://www.bing.com/ck/a?!&amp;&amp;p=abc&amp;ptn=3&amp;u=a1aHR0cHM6Ly9ydXN0LWxhbmcuZ2l0aHViLmlvL2FzeW5jLWJvb2svMDhfZWNvc3lzdGVtLzAwX2NoYXB0ZXIuaHRtbA&amp;ntb=1" h="ID=SERP,5114.2">The <strong>Async</strong> Ecosystem - Asynchronous Programming in Rust</a></h2><div class="b_caption"><p class="b_lineclamp2">The Async Ecosystem Rust currently provides only the bare essentials for writing async code. Importantly, executors, tasks, …</p></div></li><li class="b_algo" data-id iid=SERP.5330><h2><a href="https://tokio.rs/tokio/tutorial/async">Async in depth | Tokio</a></h2><div class="b_caption"><p><span class="news_dt">Feb 21, 2024</span>&nbsp;&#0183;&#32;Tokio is a runtime &amp; more …</p></div></li><li class="b_ans">not a result</li></ol>"#;

    #[test]
    fn bing_extracts_results_and_decodes_redirect_targets() {
        let hits = parse_bing(BING_PAGE).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            Hit {
                title: "The Async Ecosystem - Asynchronous Programming in Rust".into(),
                url: "https://rust-lang.github.io/async-book/08_ecosystem/00_chapter.html".into(),
                snippet: "The Async Ecosystem Rust currently provides only the bare essentials for writing async code. Importantly, executors, tasks, …".into(),
            }
        );
        assert_eq!(hits[1].url, "https://tokio.rs/tokio/tutorial/async");
        assert_eq!(hits[1].title, "Async in depth | Tokio");
        assert_eq!(
            hits[1].snippet,
            "Feb 21, 2024 · Tokio is a runtime & more …"
        );
    }

    #[test]
    fn bing_reports_a_blocked_or_redesigned_page_instead_of_no_results() {
        let error = parse_bing("<html><body>Please solve this puzzle</body></html>").unwrap_err();
        assert!(error.contains("no recognizable results"), "{error}");
        assert!(error.contains("BRAVE_API_KEY"), "{error}");
        // Bing's own "no results" page is a real empty answer.
        assert_eq!(
            parse_bing(r#"<ol id="b_results"><li class="b_no">No results found</li></ol>"#)
                .unwrap(),
            vec![]
        );
    }

    #[test]
    fn bing_targets_fall_back_to_the_link_when_they_cannot_be_decoded() {
        assert_eq!(
            bing_target("https://example.com/x"),
            "https://example.com/x"
        );
        let bad = "https://www.bing.com/ck/a?u=a1!!!notbase64";
        assert_eq!(bing_target(bad), bad);
        let not_url = "https://www.bing.com/ck/a?u=a1amF2YXNjcmlwdDphbGVydCgxKQ"; // javascript:alert(1)
        assert_eq!(bing_target(not_url), not_url);
        assert_eq!(
            bing_target("https://www.bing.com/ck/a?x=1"),
            "https://www.bing.com/ck/a?x=1"
        );
    }

    #[test]
    fn entities_decode_named_decimal_and_hex() {
        assert_eq!(
            decode_entities("a&amp;b &#0183; &#x41; &unknown; &lt;"),
            "a&b · A &unknown; <"
        );
    }

    #[test]
    fn formats_hits_with_the_backend_and_handles_none() {
        let bing = backend(&[]);
        assert_eq!(format_hits(&bing, "q", &[]), "No results for \"q\" (Bing).");
        let hits = parse_bing(BING_PAGE).unwrap();
        let text = format_hits(&bing, "rust", &hits);
        assert!(
            text.starts_with("Search results for \"rust\" (Bing):"),
            "{text}"
        );
        assert!(
            text.contains("2. Async in depth | Tokio\n   https://tokio.rs/tokio/tutorial/async"),
            "{text}"
        );
    }

    #[test]
    fn parse_truncates_to_the_requested_count() {
        let hits = backend(&[]).parse(BING_PAGE.as_bytes(), 1).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
