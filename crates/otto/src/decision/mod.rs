//! Native typed advisory decisions. Answers carry no execution or approval authority.

use std::{collections::HashMap, time::Duration};

pub mod jev;
pub mod session;
pub(crate) const YES: &str = "yes";
pub(crate) const NO: &str = "no";

/// What a question accepts as its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Exactly one of the options, each an `(id, meaning)` pair.
    Choice(Vec<(String, String)>),
    /// `yes` or `no`, with the meaning of each.
    YesNo { yes: String, no: String },
    /// Free text of at most `max_bytes` bytes, returned in `Answer::choice`.
    /// Only the session model answers it; the auxiliary model refuses.
    Text { max_bytes: usize },
}

/// One question for a [`Backend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: &'static str,
    pub instructions: String,
    pub kind: Kind,
}

/// One answer: the chosen option id (`yes` or `no` for a yes/no question) and,
/// when the backend reports it, the probability that the choice is right.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub choice: String,
    pub probability: Option<f64>,
}

impl Answer {
    /// Confidence is observational only; it cannot confer authority.
    pub fn valid_probability(&self) -> bool {
        self.probability
            .is_none_or(|p| p.is_finite() && (0.0..=1.0).contains(&p))
    }
}

/// Why no usable answer came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Timeout,
    /// The model could not be reached or refused. Carries no secret.
    Error(String),
    /// The model answered, but not in the requested form.
    Invalid(String),
}

impl Failure {
    pub(crate) fn outcome(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Error(_) => "error",
            Self::Invalid(_) => "invalid",
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Timeout => None,
            Self::Error(text) | Self::Invalid(text) => Some(text),
        }
    }
}

/// One request to a backend and what came back.
#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    /// The request as sent, for the audit log. Never holds a credential.
    pub request: String,
    /// The raw response body or text, when one arrived.
    pub response: Option<String>,
    /// Answers by question id.
    pub result: std::result::Result<HashMap<String, Answer>, Failure>,
}

/// A model that answers typed advisory questions without execution authority.
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    /// The model name stored with the advice.
    fn model(&self) -> String;
    /// Whether this is the auxiliary model rather than the session model.
    fn auxiliary(&self) -> bool;
    /// Asks `questions` about `state`, giving up after `limit`.
    async fn ask(&self, state: &str, questions: &[Question], limit: Duration) -> Exchange;
}

#[cfg(test)]
mod tests;
