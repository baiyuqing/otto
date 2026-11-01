//! The HTTP client of the auxiliary Jev model hosted by TypeSafe.
//!
//! Jev is not an LLM provider: it never runs a turn, calls a tool, or sees a
//! session transcript. It answers a fixed set of choice, yes/no and score
//! questions about one piece of text (`POST /v1/systemone`). Callers own the
//! questions and validate the answers; this module owns the wire shape,
//! authentication, the 30 second request timeout, and retries on 429/529.
//!
//! `base_url` and `api_key` are injected; nothing here reads the environment.
//! [`Error`] never carries the API key or the request body.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Request timeout of one attempt.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default backoff before each of up to 3 retries on 429/529.
const DEFAULT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// The model name sent in every request.
pub const MODEL: &str = "jev-latest";
const URL_SUFFIX: &str = "/v1/systemone";
/// The production endpoint's base URL, passed to [`Client::new`] at the
/// composition root; never read from here.
pub const BASE_URL: &str = "https://api.typesafe.ai";

/// One question on the wire: `type` is `choice`, `noul` or `score`.
#[derive(Debug, Serialize)]
pub struct WireQuestion<'a> {
    pub r#type: &'a str,
    pub instructions: &'a str,
    pub criteria: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RequestBody<'a> {
    state: &'a str,
    model: &'a str,
    questions: &'a serde_json::Map<String, serde_json::Value>,
}

/// The response `answers` object, kept exactly as returned: a map keyed by
/// question id whose values carry `type` plus type-specific fields (`noul`,
/// `choice` or `score`, and `confidence` for choice and score).
pub type AnswerMap = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Deserialize)]
struct ResponseBody {
    model: String,
    answers: AnswerMap,
}

/// A decoded 200 response. `raw` is the response body as received.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub model: String,
    pub answers: AnswerMap,
    pub raw: String,
}

/// Why a request produced no reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Status(u16),
    Timeout,
    Transport,
    InvalidResponse,
}

impl Error {
    /// A short reason with no key or body text.
    pub fn reason(&self) -> String {
        match self {
            Self::Status(code) => code.to_string(),
            Self::Timeout => "timeout".to_string(),
            Self::Transport => "transport error".to_string(),
            Self::InvalidResponse => "invalid response".to_string(),
        }
    }
}

/// The request body for `state` and `questions` (a map from question id to a
/// serialized [`WireQuestion`]), exactly as [`Client::post`] sends it.
pub fn request_body(state: &str, questions: &serde_json::Map<String, serde_json::Value>) -> String {
    serde_json::to_string(&RequestBody {
        state,
        model: MODEL,
        questions,
    })
    .expect("request body serializes")
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    backoff: Vec<Duration>,
}

impl Client {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: crate::tls_provider::http_client(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            backoff: DEFAULT_BACKOFF.to_vec(),
        }
    }

    /// Replaces the retry backoff so tests do not wait seconds for a 429.
    #[cfg(test)]
    pub fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
    }

    /// Sends `payload` (see [`request_body`]) and decodes the answer set.
    /// Retries a 429 or 529 after each backoff delay; every other non-200
    /// status is returned as is.
    pub async fn post(&self, payload: &str) -> Result<Reply, Error> {
        let url = format!("{}{URL_SUFFIX}", self.base_url);
        let mut attempt = 0usize;
        loop {
            let sent = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .timeout(REQUEST_TIMEOUT)
                .body(payload.to_owned())
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(error) if error.is_timeout() => return Err(Error::Timeout),
                Err(_) => return Err(Error::Transport),
            };
            let status = response.status().as_u16();
            if status == 200 {
                let bytes = response.bytes().await.map_err(|_| Error::Transport)?;
                let parsed: ResponseBody =
                    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)?;
                return Ok(Reply {
                    model: parsed.model,
                    answers: parsed.answers,
                    raw: String::from_utf8_lossy(&bytes).into_owned(),
                });
            }
            if (status == 429 || status == 529) && attempt < self.backoff.len() {
                tokio::time::sleep(self.backoff[attempt]).await;
                attempt += 1;
                continue;
            }
            return Err(Error::Status(status));
        }
    }
}
