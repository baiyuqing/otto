use std::sync::Arc;

use super::client::testserver::{Server, response, serve};
use super::*;
use crate::tool::testutil::{raw, run, run_cancelled};

const PAGE: &str = "<html><head><title>T</title><script>var leaked = 1;</script></head><body>\
<main><h1>Guide</h1><p>Read the <a href=\"/docs/next\">next page</a>.</p></main></body></html>";

fn runtime() -> Arc<Runtime> {
    Arc::new(Runtime {
        client: Client::new(PolicyForTests::resolver()),
    })
}

/// Namespacing for the test resolver constructor.
struct PolicyForTests;

impl PolicyForTests {
    fn resolver() -> PublicResolver {
        PublicResolver::loopback_for_tests()
    }
}

fn shared(runtime: &Arc<Runtime>, network_denied: bool, secrets: &[&str]) -> Shared {
    Shared {
        runtime: Arc::clone(runtime),
        max_output: crate::tool::testutil::MAX_OUTPUT_BYTES,
        network_denied,
        secrets: secrets.iter().map(|secret| secret.to_string()).collect(),
    }
}

fn fetch(runtime: &Arc<Runtime>) -> WebFetch {
    WebFetch {
        shared: shared(runtime, false, &[]),
    }
}

fn args(server: &Server, path: &str) -> String {
    format!(r#"{{"url":"http://page.test:{}{path}"}}"#, server.port())
}

async fn html_server() -> Server {
    serve(Arc::new(|_| {
        response(
            "200 OK",
            &[("Content-Type", "text/html; charset=utf-8")],
            PAGE.as_bytes(),
        )
    }))
    .await
}

#[tokio::test]
async fn fetch_returns_markdown_with_a_header_and_the_untrusted_notice() {
    let server = html_server().await;
    let tool = fetch(&runtime());
    let result = run(&tool, &args(&server, "/docs/start?token=hidden")).await;
    assert!(!result.is_error, "{}", result.content);
    let text = &result.content;
    assert!(text.starts_with("URL: http://page.test:"), "{text}");
    assert!(
        text.contains("/docs/start\n"),
        "the query is not echoed: {text}"
    );
    assert!(!text.contains("hidden"), "{text}");
    assert!(
        text.contains("Status: 200 · text/html; charset=utf-8"),
        "{text}"
    );
    assert!(text.contains("Untrusted web content"), "{text}");
    assert!(text.contains("# Guide"), "{text}");
    assert!(
        text.contains(&format!("(http://page.test:{}/docs/next)", server.port())),
        "links are absolute: {text}"
    );
    assert!(!text.contains("leaked"), "{text}");
}

#[tokio::test]
async fn fetch_pages_through_a_long_document_with_offset_and_limit() {
    let body = "abcdefghij".repeat(10);
    let server = serve(Arc::new(move |_| {
        response("200 OK", &[("Content-Type", "text/plain")], body.as_bytes())
    }))
    .await;
    let tool = fetch(&runtime());
    let url = format!("http://page.test:{}/", server.port());
    let first = run(&tool, &format!(r#"{{"url":"{url}","limit":30}}"#)).await;
    assert!(
        first
            .content
            .contains("100 characters · truncated; continue with offset 30"),
        "{}",
        first.content
    );
    assert!(
        first.content.ends_with(&"abcdefghij".repeat(3)),
        "{}",
        first.content
    );
    let rest = run(
        &tool,
        &format!(r#"{{"url":"{url}","offset":30,"limit":500}}"#),
    )
    .await;
    assert!(!rest.content.contains("truncated"), "{}", rest.content);
    assert!(
        rest.content.ends_with(&"abcdefghij".repeat(7)),
        "{}",
        rest.content
    );
    // The model sends 0 and [] for arguments it is not using.
    let zero = run(&tool, &format!(r#"{{"url":"{url}","offset":0,"limit":0}}"#)).await;
    assert!(!zero.is_error, "{}", zero.content);
}

#[tokio::test]
async fn fetch_caps_output_to_the_byte_budget() {
    let body = "你".repeat(100_000);
    let server = serve(Arc::new(move |_| {
        response(
            "200 OK",
            &[("Content-Type", "text/plain; charset=utf-8")],
            body.as_bytes(),
        )
    }))
    .await;
    let mut tool = fetch(&runtime());
    tool.shared.max_output = 4096;
    let result = run(
        &tool,
        &format!(
            r#"{{"url":"http://page.test:{}/","limit":1000000}}"#,
            server.port()
        ),
    )
    .await;
    assert!(
        result.content.len() <= 4096,
        "{} bytes",
        result.content.len()
    );
    assert!(
        result.content.contains("continue with offset"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn fetch_reports_http_errors_unsupported_types_and_cross_host_redirects() {
    let server = serve(Arc::new(|request| {
        if request.starts_with("GET /missing ") {
            response("404 Not Found", &[], b"nope")
        } else if request.starts_with("GET /image ") {
            response("200 OK", &[("Content-Type", "image/png")], b"\x89PNG")
        } else {
            response(
                "302 Found",
                &[("Location", "https://elsewhere.example/p?secret=1")],
                b"",
            )
        }
    }))
    .await;
    let tool = fetch(&runtime());
    let missing = run(&tool, &args(&server, "/missing")).await;
    assert!(
        missing.is_error && missing.content.contains("HTTP 404"),
        "{}",
        missing.content
    );
    let image = run(&tool, &args(&server, "/image")).await;
    assert!(
        image.is_error && image.content.contains("image/png"),
        "{}",
        image.content
    );
    let moved = run(&tool, &args(&server, "/go")).await;
    assert!(moved.is_error, "{}", moved.content);
    assert!(
        moved
            .content
            .contains("https://elsewhere.example/p?secret=1")
            && moved.content.contains("different host"),
        "{}",
        moved.content
    );
}

#[tokio::test]
async fn fetch_notes_pages_that_look_javascript_rendered() {
    let server = serve(Arc::new(|_| {
        response(
            "200 OK",
            &[("Content-Type", "text/html")],
            b"<html><body><div id=app></div><script src=app.js></script></body></html>",
        )
    }))
    .await;
    let result = run(&fetch(&runtime()), &args(&server, "/")).await;
    assert!(
        result.content.contains("rendered by JavaScript"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn fetch_reaches_new_hosts_without_approval() {
    let server = html_server().await;
    let runtime = runtime();
    let tool = fetch(&runtime);
    for host in ["page.test", "other.test"] {
        let arguments = format!(r#"{{"url":"http://{host}:{}/a?x=1"}}"#, server.port());
        assert!(tool.approval_spec(&raw(&arguments)).unwrap().is_none());
        let result = run(&tool, &arguments).await;
        assert!(!result.is_error, "{}", result.content);
    }
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn network_deny_blocks_both_tools_before_any_request() {
    let server = html_server().await;
    let runtime = runtime();
    let fetch = WebFetch {
        shared: shared(&runtime, true, &[]),
    };
    let result = run(&fetch, &args(&server, "/")).await;
    assert!(
        result.is_error && result.content.contains("[sandbox].network"),
        "{}",
        result.content
    );
    assert!(fetch.approval_spec(&raw(&args(&server, "/"))).is_err());
    let search = WebSearch {
        shared: shared(&runtime, true, &[]),
        backend: backend(&[]).with_endpoint("http://search.test/res"),
    };
    let result = run(&search, r#"{"query":"x"}"#).await;
    assert!(
        result.is_error && result.content.contains("[sandbox].network"),
        "{}",
        result.content
    );
    assert!(search.approval_spec(&raw(r#"{"query":"x"}"#)).is_err());
    assert!(server.count() == 0);
}

#[tokio::test]
async fn private_urls_and_credentials_are_refused_before_approval_or_request() {
    let runtime = runtime();
    let tool = WebFetch {
        shared: shared(&runtime, false, &["s3cr3t-value"]),
    };
    for (url, fragment) in [
        ("http://127.0.0.1:1/", "not a public address"),
        (
            "http://169.254.169.254/latest/meta-data/",
            "not a public address",
        ),
        ("file:///etc/passwd", "scheme"),
        ("https://user:pw@page.test/", "credentials"),
        ("http://page.test/?k=s3cr3t-value", "configured secret"),
        (
            "http://page.test/?k=sk-abcdefghijklmnopqrstuvwx",
            "credential",
        ),
        (
            "http://page.test/?k=ghp_abcdefghijklmnopqrstuvwxyz",
            "credential",
        ),
        ("http://page.test/?k=AKIAABCDEFGHIJKLMNOP", "credential"),
    ] {
        let arguments = format!(r#"{{"url":"{url}"}}"#);
        let result = run(&tool, &arguments).await;
        assert!(
            result.is_error && result.content.contains(fragment),
            "{url}: {}",
            result.content
        );
        assert!(
            tool.approval_spec(&raw(&arguments)).is_err(),
            "{url} must not reach an approval prompt"
        );
    }
    assert!(
        !run(&tool, r#"{"url":"http://page.test/ok","offset":0}"#)
            .await
            .content
            .contains("credential")
    );
}

#[tokio::test]
async fn a_hostname_that_resolves_to_a_private_address_is_blocked_by_the_system_resolver() {
    let runtime = Arc::new(Runtime {
        client: Client::new(PublicResolver::system()),
    });
    let result = run(&fetch(&runtime), r#"{"url":"http://localhost:9/"}"#).await;
    assert!(
        result.is_error && result.content.contains("not a public address"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn cancellation_is_reported_without_a_request() {
    let server = html_server().await;
    let tool = fetch(&runtime());
    let result = run_cancelled(&tool, &args(&server, "/")).await;
    assert!(
        result.is_error && result.content == "context canceled",
        "{}",
        result.content
    );
    assert!(server.count() == 0);
}

#[tokio::test]
async fn invalid_arguments_are_in_band_errors() {
    let tool = fetch(&runtime());
    for arguments in [r#"{}"#, r#"{"url":""}"#, r#"{"url":5}"#] {
        let result = run(&tool, arguments).await;
        assert!(result.is_error, "{arguments}: {}", result.content);
    }
}

// ----- web_search

const SEARCH_JSON: &str = r#"{"web":{"results":[
  {"title":"Rust <strong>book</strong>","url":"https://doc.rust-lang.org/book/","description":"Learn Rust"},
  {"title":"Crates","url":"https://crates.io/","description":"Packages"}]}}"#;

/// A backend chosen from `vars` the way the runner chooses it.
fn backend(vars: &[(&str, &str)]) -> search::Backend {
    let vars: std::collections::HashMap<String, String> = vars
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    search::Backend::from_env(&|name| vars.get(name).cloned())
}

fn search_tool_with(
    runtime: &Arc<Runtime>,
    server: &Server,
    backend: search::Backend,
) -> WebSearch {
    let secrets = backend.secrets();
    let backend = backend.with_endpoint(&format!(
        "http://search.test:{}/res/v1/web/search",
        server.port()
    ));
    WebSearch {
        shared: Shared {
            runtime: Arc::clone(runtime),
            max_output: crate::tool::testutil::MAX_OUTPUT_BYTES,
            network_denied: false,
            secrets,
        },
        backend,
    }
}

/// A Brave-backed tool whose key is `key`.
fn search_tool(runtime: &Arc<Runtime>, server: &Server, key: &str) -> WebSearch {
    search_tool_with(runtime, server, backend(&[("BRAVE_API_KEY", key)]))
}

async fn search_server() -> Server {
    serve(Arc::new(|_| {
        response(
            "200 OK",
            &[("Content-Type", "application/json")],
            SEARCH_JSON.as_bytes(),
        )
    }))
    .await
}

#[tokio::test]
async fn search_sends_the_key_as_a_header_and_never_returns_it() {
    let server = search_server().await;
    let runtime = runtime();
    let tool = search_tool(&runtime, &server, "key-123456-abc");
    let result = run(&tool, r#"{"query":"rust & go","count":2}"#).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .contains("1. Rust book\n   https://doc.rust-lang.org/book/"),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("Untrusted web content"),
        "{}",
        result.content
    );
    assert!(!result.content.contains("key-123456-abc"));
    let request = server.requests()[0].head.to_ascii_lowercase();
    assert!(
        request.contains("x-subscription-token: key-123456-abc"),
        "{request}"
    );
    assert!(
        request.contains("count=2") && request.contains("q=rust+%26+go"),
        "{request}"
    );
}

#[tokio::test]
async fn search_validates_arguments_and_runs_without_approval() {
    let server = search_server().await;
    let runtime = runtime();
    let tool = search_tool(&runtime, &server, "key-123456-abc");
    for arguments in [r#"{"query":""}"#, r#"{"query":"   "}"#] {
        assert!(run(&tool, arguments).await.is_error, "{arguments}");
    }
    let long = format!(r#"{{"query":"{}"}}"#, "a".repeat(MAX_QUERY_CHARS + 1));
    assert!(run(&tool, &long).await.content.contains("longer"));

    assert!(
        tool.approval_spec(&raw(r#"{"query":"other"}"#))
            .unwrap()
            .is_none()
    );
    assert!(
        !run(&tool, r#"{"query":"other","count":999}"#)
            .await
            .is_error
    );
    assert!(server.requests()[0].head.contains("count=20"));
}

#[tokio::test]
async fn search_refuses_a_credential_in_the_query_and_reports_service_errors() {
    let server = serve(Arc::new(|_| {
        response("429 Too Many Requests", &[], b"slow down")
    }))
    .await;
    let runtime = runtime();
    let tool = search_tool(&runtime, &server, "key-123456-abc");
    let result = run(&tool, r#"{"query":"key-123456-abc"}"#).await;
    assert!(
        result.is_error && result.content.contains("configured secret"),
        "{}",
        result.content
    );
    let failed = run(&tool, r#"{"query":"ok"}"#).await;
    assert!(
        failed.is_error && failed.content.contains("HTTP 429"),
        "{}",
        failed.content
    );
}

#[test]
fn both_tools_are_always_registered_and_the_backend_comes_from_the_environment() {
    let runtime = Runtime::new();
    let names = |vars: &'static [(&'static str, &'static str)], hosted: bool| -> Vec<String> {
        tools(
            &runtime,
            1000,
            false,
            &[],
            &|name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            },
            hosted,
        )
        .iter()
        .map(|tool| tool.definition().name)
        .collect()
    };
    assert_eq!(names(&[], false), vec!["web_fetch", "web_search"]);
    assert_eq!(
        names(&[("BRAVE_API_KEY", "k1234567")], false),
        vec!["web_fetch", "web_search"]
    );
    // A provider-hosted search replaces only the keyless backend.
    assert_eq!(names(&[], true), vec!["web_fetch"]);
    assert_eq!(
        names(&[("BRAVE_API_KEY", "k1234567")], true),
        vec!["web_fetch", "web_search"]
    );
    assert!(search_is_keyless(&|_| None));
    assert!(!search_is_keyless(
        &|name| (name == "BRAVE_API_KEY").then(|| "k".to_string())
    ));
}

#[tokio::test]
async fn google_sends_key_and_engine_as_parameters_clamps_count_and_never_returns_them() {
    let server = serve(Arc::new(|_| {
        response(
            "200 OK",
            &[("Content-Type", "application/json")],
            br#"{"items":[{"title":"Rust","link":"https://www.rust-lang.org/","snippet":"A language"}]}"#,
        )
    }))
    .await;
    let runtime = runtime();
    let google = backend(&[
        ("GOOGLE_API_KEY", "gkey-1234567"),
        ("GOOGLE_SEARCH_ENGINE_ID", "cx-987654"),
    ]);
    let tool = search_tool_with(&runtime, &server, google);
    let result = run(&tool, r#"{"query":"rust","count":50}"#).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("(Google)") && result.content.contains("1. Rust"),
        "{}",
        result.content
    );
    assert!(!result.content.contains("gkey-1234567") && !result.content.contains("cx-987654"));
    let request = server.requests()[0].head.clone();
    assert!(
        request.contains("key=gkey-1234567") && request.contains("cx=cx-987654"),
        "{request}"
    );
    assert!(
        request.contains("num=10"),
        "Google returns at most 10: {request}"
    );
    // The engine id is as secret as the key.
    let leak = run(&tool, r#"{"query":"cx-987654"}"#).await;
    assert!(
        leak.is_error && leak.content.contains("configured secret"),
        "{}",
        leak.content
    );
}

#[tokio::test]
async fn bing_needs_no_key_and_sends_no_credentials() {
    const PAGE: &str = r#"<ol id="b_results"><li class="b_algo"><h2><a href="https://www.bing.com/ck/a?!&amp;&amp;u=a1aHR0cHM6Ly90b2tpby5ycy90b2tpby90dXRvcmlhbC9hc3luYw&amp;ntb=1">Tokio</a></h2><div class="b_caption"><p>A runtime</p></div></li></ol>"#;
    let server = serve(Arc::new(|_| {
        response(
            "200 OK",
            &[("Content-Type", "text/html; charset=utf-8")],
            PAGE.as_bytes(),
        )
    }))
    .await;
    let runtime = runtime();
    let tool = search_tool_with(&runtime, &server, backend(&[]));
    let result = run(&tool, r#"{"query":"tokio"}"#).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("(Bing)"), "{}", result.content);
    assert!(
        result
            .content
            .contains("1. Tokio\n   https://tokio.rs/tokio/tutorial/async\n   A runtime"),
        "{}",
        result.content
    );
    let request = server.requests()[0].head.to_ascii_lowercase();
    assert!(request.contains("accept: text/html"), "{request}");
    for forbidden in ["x-subscription-token", "authorization:", "cookie:"] {
        assert!(!request.contains(forbidden), "{request}");
    }
}

#[tokio::test]
async fn a_bing_page_that_cannot_be_read_is_an_error_not_an_empty_answer() {
    let server = serve(Arc::new(|_| {
        response(
            "200 OK",
            &[("Content-Type", "text/html")],
            b"<html>captcha</html>",
        )
    }))
    .await;
    let tool = search_tool_with(&runtime(), &server, backend(&[]));
    let result = run(&tool, r#"{"query":"x"}"#).await;
    assert!(
        result.is_error && result.content.contains("no recognizable results"),
        "{}",
        result.content
    );
}

#[test]
fn definitions_are_strict_schemas() {
    for definition in [fetch_definition(), search_definition()] {
        let schema: serde_json::Value =
            serde_json::from_str(definition.parameters.as_ref().unwrap().get()).unwrap();
        assert_eq!(schema["additionalProperties"], false, "{}", definition.name);
        assert_eq!(schema["type"], "object");
    }
}

#[tokio::test]
async fn explicit_large_fetch_limit_still_returns_a_bounded_page() {
    let body = "x".repeat(6000);
    let server = serve(Arc::new(move |_| {
        response("200 OK", &[("Content-Type", "text/plain")], body.as_bytes())
    }))
    .await;
    let tool = fetch(&runtime());
    let url = format!("http://page.test:{}/", server.port());
    let result = run(&tool, &format!(r#"{{"url":"{url}","limit":50000}}"#)).await;
    assert!(result.content.contains("continue with offset 4096"));
    assert!(result.content.ends_with(&"x".repeat(4096)));
    assert!(result.content.len() < 4600);
}
