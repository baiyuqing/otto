//! The HTTP client the web tools share.
//!
//! One `reqwest` client with rustls, no cookies, no proxies, no automatic
//! redirects and a resolver that only returns public addresses
//! ([`super::policy::PublicResolver`]). Redirects are followed here, by hand,
//! only while they stay on the same host: a redirect to another host is
//! reported to the caller, which has to ask for it as a new call so that the
//! new destination goes through approval.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::policy::{self, PublicResolver};

/// The most response body bytes read from one page.
pub(super) const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// The most same-host redirects followed in one call.
const MAX_REDIRECTS: usize = 5;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A completed request.
#[derive(Debug)]
pub(super) struct Page {
    pub final_url: Url,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// The body hit [`MAX_BODY_BYTES`] and the rest was dropped.
    pub body_truncated: bool,
}

#[derive(Debug)]
pub(super) enum Response {
    Page(Page),
    /// The server redirected to a different host. Nothing was requested from it.
    CrossHostRedirect {
        to: Url,
        status: u16,
    },
}

/// The shared client. The `reqwest` client is built on first use, because
/// building it loads the system trust roots and a runner that never fetches
/// should not pay for that.
#[derive(Clone)]
pub(super) struct Client {
    resolver: PublicResolver,
    http: Arc<OnceLock<Result<reqwest::Client, String>>>,
}

impl Client {
    pub(super) fn new(resolver: PublicResolver) -> Self {
        Self {
            resolver,
            http: Arc::new(OnceLock::new()),
        }
    }

    fn http(&self) -> Result<&reqwest::Client, String> {
        self.http
            .get_or_init(|| {
                otto_platform::tls_provider::http_client_builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .no_proxy()
                    .dns_resolver(Arc::new(self.resolver.clone()))
                    .connect_timeout(CONNECT_TIMEOUT)
                    .user_agent(format!("otto/{}", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|error| format!("create web client: {error}"))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// GETs `url` with `accept`, following same-host redirects.
    pub(super) async fn get(
        &self,
        url: Url,
        accept: &str,
        extra_headers: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<Response, String> {
        let work = self.get_inner(url, accept, extra_headers, cancel);
        tokio::select! {
            () = cancel.cancelled() => Err(super::super::CONTEXT_CANCELED.to_string()),
            result = tokio::time::timeout(TOTAL_TIMEOUT, work) => match result {
                Ok(result) => result,
                Err(_) => Err(format!("request timed out after {}s", TOTAL_TIMEOUT.as_secs())),
            },
        }
    }

    async fn get_inner(
        &self,
        mut url: Url,
        accept: &str,
        extra_headers: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<Response, String> {
        let origin_host = policy::host_key(&url);
        for _ in 0..=MAX_REDIRECTS {
            let mut request = self.http()?.get(url.clone()).header(ACCEPT, accept);
            for (name, value) in extra_headers {
                request = request.header(*name, *value);
            }
            let mut response = request.send().await.map_err(describe_error)?;
            let status = response.status();
            if status.is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                let location = location
                    .to_str()
                    .map_err(|_| "redirect location is not valid text".to_string())?;
                let next = url
                    .join(location)
                    .map_err(|error| format!("invalid redirect location: {error}"))?;
                let next = policy::parse_url(next.as_str())?;
                match redirect_step(&url, &next, &origin_host)? {
                    Step::Follow => {
                        url = next;
                        continue;
                    }
                    Step::CrossHost => {
                        return Ok(Response::CrossHostRedirect {
                            to: next,
                            status: status.as_u16(),
                        });
                    }
                }
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let mut body = Vec::new();
            let mut body_truncated = false;
            loop {
                if cancel.is_cancelled() {
                    return Err(super::super::CONTEXT_CANCELED.to_string());
                }
                match response.chunk().await.map_err(describe_error)? {
                    None => break,
                    Some(chunk) => {
                        let room = MAX_BODY_BYTES - body.len();
                        if chunk.len() > room {
                            body.extend_from_slice(&chunk[..room]);
                            body_truncated = true;
                            break;
                        }
                        body.extend_from_slice(&chunk);
                    }
                }
            }
            return Ok(Response::Page(Page {
                final_url: url,
                status: status.as_u16(),
                content_type,
                body,
                body_truncated,
            }));
        }
        Err(format!("stopped after {MAX_REDIRECTS} redirects"))
    }
}

enum Step {
    Follow,
    CrossHost,
}

/// What to do with a redirect from `current` to `next`, given the host of the
/// first request.
fn redirect_step(current: &Url, next: &Url, origin_host: &Option<String>) -> Result<Step, String> {
    if current.scheme() == "https" && next.scheme() == "http" {
        return Err(format!(
            "refused redirect from https to http: {}",
            redacted(next)
        ));
    }
    if policy::host_key(next) != *origin_host {
        return Ok(Step::CrossHost);
    }
    Ok(Step::Follow)
}

/// A user-visible request error without the URL's query string, which can
/// carry tokens.
fn describe_error(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    if error.is_timeout() {
        return "request timed out".to_string();
    }
    text
}

/// `url` without its query or fragment.
pub(super) fn redacted(url: &Url) -> String {
    let mut bare = url.clone();
    bare.set_query(None);
    bare.set_fragment(None);
    bare.to_string()
}

#[cfg(test)]
pub(super) mod testserver {
    //! Canned-response helpers over the shared loopback server.

    use std::sync::Arc;

    pub(crate) use otto_faults::http::Server;

    /// One canned response: raw bytes written after the request is read.
    pub(crate) type Handler = Arc<dyn Fn(&str) -> Vec<u8> + Send + Sync>;

    pub(crate) fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\n").into_bytes();
        for (name, value) in headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        out.extend_from_slice(body);
        out
    }

    /// Answers each request with `handler(request head)`.
    pub(crate) async fn serve(handler: Handler) -> Server {
        otto_faults::http::spawn(move |request| handler(&request.head)).await
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::{response, serve};
    use super::*;

    fn client() -> Client {
        Client::new(PublicResolver::loopback_for_tests())
    }

    fn url(server: &testserver::Server, path: &str) -> Url {
        Url::parse(&format!("http://page.test:{}{path}", server.port())).unwrap()
    }

    #[tokio::test]
    async fn fetches_a_page_and_sends_no_credentials_or_cookies() {
        let server = serve(Arc::new(|_| {
            response(
                "200 OK",
                &[("Content-Type", "text/plain"), ("Set-Cookie", "a=b")],
                b"hello",
            )
        }))
        .await;
        let client = client();
        let cancel = CancellationToken::new();
        let first = client
            .get(url(&server, "/"), "text/plain", &[], &cancel)
            .await;
        let Ok(Response::Page(page)) = first else {
            panic!("{first:?}")
        };
        assert_eq!(page.status, 200);
        assert_eq!(page.body, b"hello");
        assert_eq!(page.content_type, "text/plain");
        client
            .get(url(&server, "/"), "text/plain", &[], &cancel)
            .await
            .unwrap();
        let requests = server.requests();
        let second = requests[1].head.to_ascii_lowercase();
        assert!(second.starts_with("get / http/1.1"), "{second}");
        assert!(second.contains("user-agent: otto/"), "{second}");
        assert!(second.contains("accept: text/plain"), "{second}");
        for forbidden in ["cookie:", "authorization:"] {
            assert!(!second.contains(forbidden), "{second}");
        }
    }

    #[tokio::test]
    async fn follows_same_host_redirects_and_stops_at_the_limit() {
        let server = serve(Arc::new(|request| {
            if request.starts_with("GET /a ") {
                response("302 Found", &[("Location", "/b")], b"")
            } else if request.starts_with("GET /b ") {
                response("200 OK", &[("Content-Type", "text/plain")], b"done")
            } else {
                response("302 Found", &[("Location", "/loop")], b"")
            }
        }))
        .await;
        let client = client();
        let cancel = CancellationToken::new();
        let Ok(Response::Page(page)) = client.get(url(&server, "/a"), "*/*", &[], &cancel).await
        else {
            panic!("expected a page")
        };
        assert_eq!(page.body, b"done");
        assert!(page.final_url.path() == "/b");
        let error = client
            .get(url(&server, "/loop"), "*/*", &[], &cancel)
            .await
            .unwrap_err();
        assert!(error.contains("redirects"), "{error}");
    }

    #[tokio::test]
    async fn a_cross_host_redirect_is_reported_and_not_followed() {
        let server = serve(Arc::new(|_| {
            response(
                "301 Moved",
                &[("Location", "https://other.example/x?q=1")],
                b"",
            )
        }))
        .await;
        let client = client();
        let result = client
            .get(url(&server, "/"), "*/*", &[], &CancellationToken::new())
            .await
            .unwrap();
        match result {
            Response::CrossHostRedirect { to, status } => {
                assert_eq!(to.as_str(), "https://other.example/x?q=1");
                assert_eq!(status, 301);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_refused() {
        let server = serve(Arc::new(|_| {
            response(
                "302 Found",
                &[("Location", "http://169.254.169.254/latest/meta-data")],
                b"",
            )
        }))
        .await;
        let error = client()
            .get(url(&server, "/"), "*/*", &[], &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("not a public address"), "{error}");
    }

    #[test]
    fn redirect_steps_follow_same_host_and_refuse_a_scheme_downgrade() {
        let u = |text: &str| Url::parse(text).unwrap();
        let origin = Some("secure.example".to_string());
        assert!(matches!(
            redirect_step(
                &u("https://secure.example/a"),
                &u("https://secure.example/b"),
                &origin
            ),
            Ok(Step::Follow)
        ));
        assert!(matches!(
            redirect_step(
                &u("http://secure.example/a"),
                &u("https://secure.example/b"),
                &origin
            ),
            Ok(Step::Follow)
        ));
        assert!(matches!(
            redirect_step(
                &u("https://secure.example/a"),
                &u("https://other.example/b"),
                &origin
            ),
            Ok(Step::CrossHost)
        ));
        let error = redirect_step(
            &u("https://secure.example/a"),
            &u("http://secure.example/b?token=s"),
            &origin,
        )
        .err()
        .expect("downgrade is refused");
        assert!(
            error.contains("https to http") && !error.contains("token"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn the_body_is_capped() {
        let big = vec![b'x'; MAX_BODY_BYTES + 5000];
        let server = serve(Arc::new(move |_| {
            response("200 OK", &[("Content-Type", "text/plain")], &big)
        }))
        .await;
        let Ok(Response::Page(page)) = client()
            .get(url(&server, "/"), "*/*", &[], &CancellationToken::new())
            .await
        else {
            panic!("expected a page")
        };
        assert_eq!(page.body.len(), MAX_BODY_BYTES);
        assert!(page.body_truncated);
    }

    #[tokio::test]
    async fn cancellation_stops_a_stalled_request() {
        let server =
            otto_faults::http::spawn_scripted(vec![otto_faults::http::Script::stall_before_head()])
                .await;
        let port = server.port();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stop.cancel();
        });
        let url = Url::parse(&format!("http://stall.test:{port}/")).unwrap();
        let error = client().get(url, "*/*", &[], &cancel).await.unwrap_err();
        assert_eq!(error, "context canceled");
    }

    #[tokio::test]
    async fn a_name_that_resolves_privately_fails_without_connecting() {
        let client = Client::new(PublicResolver::system());
        let url = Url::parse("http://localhost:9/").unwrap();
        let error = client
            .get(url, "*/*", &[], &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("not a public address"), "{error}");
    }
}
