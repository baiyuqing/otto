//! Advisory review hints on memory candidates.
//!
//! Before a candidate from the `remember` tool is queued,
//! [`Advisor::propose`] asks a model three questions about it: where it should
//! apply (scope), how it relates to the standing records of the user scope
//! (new, duplicate, conflict), and whether it is a durable habit or a one-off
//! request. The answers are stored in the candidate's metadata under the
//! `advice.` prefix and shown under the candidate by every review surface
//! ([`advice_line`]). The advice never accepts, rejects, or edits a candidate;
//! a person still decides.
//!
//! Two backends answer through the [`Backend`] contract: the session model
//! ([`session`] holds its prompt and parser; the backend itself is built at the
//! composition root) and the auxiliary Jev model ([`jev`]), which is used
//! when `TYPESAFE_API_KEY` is set. Both are limited to [`TIMEOUT`].
//!
//! Order: the proposal is first checked without writing
//! ([`super::Service::check_proposal`]); a proposal that would be refused
//! reaches no model. A timeout, an error, or an invalid answer queues the
//! candidate without advice. Every request is appended to the [`audit`] log.
//!
//! Concurrency: [`Advisor::propose`] asks for all of its requests at once and
//! queues the candidates one after another, in request order.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use tokio_util::sync::CancellationToken;

use super::{
    Candidate, CandidateAction, NAMESPACE_USER, NAMESPACE_WORKSPACE, ProposeRequest, Record,
    Result, Scope, Service,
};

#[path = "audit_contracts.rs"]
pub mod audit;
pub mod sqlite;

/// The longest a model has to answer for one candidate.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// Metadata key prefix of stored advice.
const PREFIX: &str = "advice.";
/// Longest model name stored, in bytes.
const MAX_MODEL_BYTES: usize = 128;

pub(crate) const SCOPE: &str = "scope";
pub(crate) const RELATION: &str = "relation";
pub(crate) const DURABLE: &str = "durable";
const ALL_WORKSPACES: &str = "all_workspaces";
const THIS_WORKSPACE: &str = "this_workspace";
const NEW: &str = "new";
const DUPLICATE: &str = "duplicate";
const CONFLICT: &str = "conflict";
pub(crate) const YES: &str = "yes";
pub(crate) const NO: &str = "no";

pub use crate::decision::{Answer, Backend, Exchange, Failure, Kind, Question};

/// Where a candidate should apply, as advised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeAdvice {
    User,
    Workspace,
}

/// How a candidate relates to the standing records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Relation {
    New,
    Duplicate(String),
    Conflict(String),
}

/// The three answers about one candidate.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Advice {
    pub model: String,
    pub auxiliary: bool,
    pub scope: ScopeAdvice,
    pub scope_p: Option<f64>,
    pub relation: Relation,
    pub relation_p: Option<f64>,
    pub durable: bool,
    pub durable_p: Option<f64>,
}

fn key_of(name: &str) -> String {
    format!("{PREFIX}{name}")
}

impl Advice {
    /// The metadata entries that store this advice.
    fn metadata(&self) -> BTreeMap<String, String> {
        let mut entries = BTreeMap::new();
        let mut put = |name: &str, value: String| {
            entries.insert(key_of(name), value);
        };
        put("model", self.model.clone());
        put("auxiliary", self.auxiliary.to_string());
        put(
            "scope",
            match self.scope {
                ScopeAdvice::User => NAMESPACE_USER,
                ScopeAdvice::Workspace => NAMESPACE_WORKSPACE,
            }
            .to_owned(),
        );
        let (relation, relation_key) = match &self.relation {
            Relation::New => (NEW, ""),
            Relation::Duplicate(key) => (DUPLICATE, key.as_str()),
            Relation::Conflict(key) => (CONFLICT, key.as_str()),
        };
        put("relation", relation.to_owned());
        if !relation_key.is_empty() {
            put("relation_key", relation_key.to_owned());
        }
        put("durable", self.durable.to_string());
        for (name, probability) in [
            ("scope_p", self.scope_p),
            ("relation_p", self.relation_p),
            ("durable_p", self.durable_p),
        ] {
            if let Some(probability) = probability {
                put(name, format!("{probability:.2}"));
            }
        }
        entries
    }

    /// Reads advice back from stored metadata; `None` when there is none or
    /// it is malformed.
    fn from_metadata(metadata: &BTreeMap<String, String>) -> Option<Self> {
        let get = |name: &str| metadata.get(&key_of(name)).map(String::as_str);
        let probability = |name: &str| {
            get(name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
        };
        let relation_key = get("relation_key").unwrap_or_default().to_owned();
        Some(Self {
            model: get("model")?.to_owned(),
            auxiliary: get("auxiliary")? == "true",
            scope: match get("scope")? {
                NAMESPACE_USER => ScopeAdvice::User,
                NAMESPACE_WORKSPACE => ScopeAdvice::Workspace,
                _ => return None,
            },
            scope_p: probability("scope_p"),
            relation: match get("relation")? {
                NEW => Relation::New,
                DUPLICATE if !relation_key.is_empty() => Relation::Duplicate(relation_key),
                CONFLICT if !relation_key.is_empty() => Relation::Conflict(relation_key),
                _ => return None,
            },
            relation_p: probability("relation_p"),
            durable: match get("durable")? {
                "true" => true,
                "false" => false,
                _ => return None,
            },
            durable_p: probability("durable_p"),
        })
    }
}

/// The one line every review surface shows under a candidate, or `None` when
/// `proposed` carries no (valid) advice.
///
/// Session model: `Advice (session model gpt-x): all workspaces · duplicates
/// plan-first · one-off request`. Auxiliary model: `Advice (auxiliary model
/// jev-latest): all workspaces 91% · new 84% · durable 77%`. When the advised
/// scope differs from the candidate's own, the scope part ends with
/// ` (proposed: workspace)` or ` (proposed: user)`.
pub fn advice_line(proposed: &Record) -> Option<String> {
    let advice = Advice::from_metadata(&proposed.metadata)?;
    let percent = |probability: Option<f64>| {
        probability.map_or_else(String::new, |p| format!(" {}%", (p * 100.0).round() as i64))
    };
    let mut scope = match advice.scope {
        ScopeAdvice::User => "all workspaces",
        ScopeAdvice::Workspace => "this workspace",
    }
    .to_owned();
    scope.push_str(&percent(advice.scope_p));
    let proposed_scope = match proposed.scope.namespace.as_str() {
        NAMESPACE_USER => Some(ScopeAdvice::User),
        NAMESPACE_WORKSPACE => Some(ScopeAdvice::Workspace),
        _ => None,
    };
    if let Some(own) = proposed_scope
        && own != advice.scope
    {
        let _ = write!(scope, " (proposed: {})", proposed.scope.namespace);
    }
    let relation = match &advice.relation {
        Relation::New => "new".to_owned(),
        Relation::Duplicate(key) => format!("duplicates {key}"),
        Relation::Conflict(key) => format!("conflicts with {key}"),
    } + &percent(advice.relation_p);
    let durable = if advice.durable {
        "durable"
    } else {
        "one-off request"
    }
    .to_owned()
        + &percent(advice.durable_p);
    let model = if advice.auxiliary {
        "auxiliary model"
    } else {
        "session model"
    };
    Some(format!(
        "Advice ({model} {}): {scope} · {relation} · {durable}",
        advice.model
    ))
}

/// The state text both backends receive: the candidate and the standing
/// records of the user scope.
fn describe(request: &ProposeRequest, standing: &[Record]) -> String {
    let scope = &request.scope.namespace;
    let action = match request.action {
        CandidateAction::Update => "update",
        _ => "create",
    };
    let mut state = format!(
        "Candidate memory proposed for human review\naction: {action}\nkind: \
            {}\nkey: {}\nscope: {scope}\ntext: {}\nreason (evidence quote): \
            {}\n\nStanding records of the user scope (apply in every workspace):",
        request.kind, request.key, request.text, request.reason
    );
    if standing.is_empty() {
        state.push_str("\n(none)");
    }
    for record in standing {
        let _ = write!(state, "\n- key: {} text: {}", record.key, record.text);
    }
    state
}

fn questions(standing: &[&Record]) -> Vec<Question> {
    let mut questions = vec![Question {
        id: SCOPE,
        instructions: "Where should this memory apply?".to_owned(),
        kind: Kind::Choice(vec![
            (
                ALL_WORKSPACES.to_owned(),
                "how the user wants the agent to work in every project: working habits, communication preferences, standing rules.".to_owned(),
            ),
            (
                THIS_WORKSPACE.to_owned(),
                "a fact or convention that belongs to the current project only.".to_owned(),
            ),
        ]),
    }];
    if !standing.is_empty() {
        let mut options = vec![(
            NEW.to_owned(),
            "no standing record says or contradicts the same thing.".to_owned(),
        )];
        for record in standing {
            options.push((
                format!("{DUPLICATE}:{}", record.key),
                format!(
                    "says the same as the standing record with key {}.",
                    record.key
                ),
            ));
            options.push((
                format!("{CONFLICT}:{}", record.key),
                format!("contradicts the standing record with key {}.", record.key),
            ));
        }
        questions.push(Question {
            id: RELATION,
            instructions: "How does the candidate relate to the standing records?".to_owned(),
            kind: Kind::Choice(options),
        });
    }
    questions.push(Question {
        id: DURABLE,
        instructions: "Is the candidate a durable habit or standing rule, or a one-off request for the current task?".to_owned(),
        kind: Kind::YesNo {
            yes: "a durable habit or standing rule that applies in later sessions.".to_owned(),
            no: "a one-off request for the current task.".to_owned(),
        },
    });
    questions
}

fn relation_of(choice: &str) -> Option<Relation> {
    if choice == NEW {
        Some(Relation::New)
    } else if let Some(key) = choice.strip_prefix("duplicate:") {
        Some(Relation::Duplicate(key.to_owned()))
    } else {
        choice
            .strip_prefix("conflict:")
            .map(|key| Relation::Conflict(key.to_owned()))
    }
}

/// Turns validated answers into advice; `None` when an expected answer is
/// missing, which the caller treats as an invalid exchange.
fn interpret(model: String, auxiliary: bool, answers: &HashMap<String, Answer>) -> Option<Advice> {
    if !answers.values().all(Answer::valid_probability) {
        return None;
    }
    let scope = answers.get(SCOPE)?;
    let durable = answers.get(DURABLE)?;
    let relation = match answers.get(RELATION) {
        None => (Relation::New, None),
        Some(answer) => (relation_of(&answer.choice)?, answer.probability),
    };
    Some(Advice {
        model: truncate(model, MAX_MODEL_BYTES),
        auxiliary,
        scope: match scope.choice.as_str() {
            ALL_WORKSPACES => ScopeAdvice::User,
            THIS_WORKSPACE => ScopeAdvice::Workspace,
            _ => return None,
        },
        scope_p: scope.probability,
        relation: relation.0,
        relation_p: relation.1,
        durable: match durable.choice.as_str() {
            YES => true,
            NO => false,
            _ => return None,
        },
        durable_p: durable.probability,
    })
}

fn truncate(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// One advice request that was sent, with what the audit row needs.
struct Asked {
    advice: Option<Advice>,
    row: audit::NewRow,
}

/// Asks a [`Backend`] for advice and queues candidates with it.
pub struct Advisor {
    backend: Box<dyn Backend>,
    audit: Option<Arc<dyn audit::AuditStore>>,
    limit: Duration,
}

impl Advisor {
    /// Compatibility constructor for callers using the current SQLite store.
    pub fn new(backend: Box<dyn Backend>, audit: Option<Arc<dyn audit::AuditStore>>) -> Self {
        Self::new_with_audit(backend, audit)
    }

    /// Builds an advisor with an injected typed audit repository.
    pub(crate) fn new_with_audit(
        backend: Box<dyn Backend>,
        audit: Option<Arc<dyn audit::AuditStore>>,
    ) -> Self {
        Self {
            backend,
            audit,
            limit: TIMEOUT,
        }
    }

    /// Replaces the time limit so a test does not wait 20 seconds.
    #[cfg(test)]
    pub(crate) fn with_limit(mut self, limit: Duration) -> Self {
        self.limit = limit;
        self
    }

    /// Whether the auxiliary model answers.
    #[cfg(test)]
    pub(crate) fn is_auxiliary(&self) -> bool {
        self.backend.auxiliary()
    }

    /// Queues each request as a candidate, with advice where a model gave a
    /// valid answer within the limit. Returns one result per request, in
    /// order, exactly as [`Service::propose`] would, or `None` without
    /// queueing anything when `cancel` fires first. Forget requests and
    /// requests [`Service::check_proposal`] refuses reach no model.
    pub async fn propose(
        &self,
        service: &Service,
        user_scope: &Scope,
        requests: Vec<ProposeRequest>,
        cancel: &CancellationToken,
    ) -> Option<Vec<Result<Vec<Candidate>>>> {
        let standing = service.standing_records(user_scope).unwrap_or_default();
        let asked = join_all(
            requests
                .iter()
                .map(|request| self.advise(service, request, &standing, cancel)),
        )
        .await;
        if cancel.is_cancelled() {
            return None;
        }
        let mut results = Vec::with_capacity(requests.len());
        for (request, asked) in requests.into_iter().zip(asked) {
            let Some(asked) = asked else {
                results.push(service.propose(&request));
                continue;
            };
            let mut result = match &asked.advice {
                Some(advice) => {
                    let mut advised = request.clone();
                    advised.metadata.extend(advice.metadata());
                    service.propose(&advised)
                }
                None => service.propose(&request),
            };
            // The advice values come from fixed options and a model name, but
            // the store's content guard has the last word on metadata.
            if result.is_err() && asked.advice.is_some() {
                result = service.propose(&request);
            }
            let candidate_id = result
                .as_ref()
                .ok()
                .and_then(|candidates| candidates.first())
                .map(|candidate| candidate.id.clone());
            if let Some(audit) = &self.audit {
                // An audit row that cannot be written never fails the proposal.
                let _ = audit.append(&asked.row, candidate_id.as_deref());
            }
            results.push(result);
        }
        Some(results)
    }

    /// Asks the backend about `request`; `None` when no model call is made.
    async fn advise(
        &self,
        service: &Service,
        request: &ProposeRequest,
        standing: &[Record],
        cancel: &CancellationToken,
    ) -> Option<Asked> {
        if request.action == CandidateAction::Forget || service.check_proposal(request).is_err() {
            return None;
        }
        let comparable: Vec<&Record> = standing
            .iter()
            .filter(|record| record.id != request.target_id)
            .collect();
        let questions = questions(&comparable);
        let state = describe(request, standing);
        let started = Instant::now();
        let exchange = tokio::select! {
            exchange = self.backend.ask(&state, &questions, self.limit) => exchange,
            () = cancel.cancelled() => return None,
        };
        let latency = started.elapsed();
        let model = self.backend.model();
        let auxiliary = self.backend.auxiliary();
        let (advice, failure) = match exchange.result {
            Ok(answers) => match interpret(model.clone(), auxiliary, &answers) {
                Some(advice) => (Some(advice), None),
                None => (
                    None,
                    Some(Failure::Invalid("an expected answer is missing".to_owned())),
                ),
            },
            Err(failure) => (None, Some(failure)),
        };
        Some(Asked {
            advice,
            row: audit::NewRow {
                model,
                auxiliary,
                scope: request.scope.namespace.clone(),
                request: exchange.request,
                response: exchange.response,
                outcome: failure.as_ref().map_or("ok", Failure::outcome),
                error: failure.as_ref().and_then(Failure::text).map(str::to_owned),
                latency_ms: latency.as_millis() as i64,
            },
        })
    }
}

#[cfg(test)]
mod tests;

/// Candidates for the review-surface tests of other modules.
#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use super::*;

    /// The advice line [`candidate`] stores when `advised`, for a candidate
    /// of the user scope.
    pub const LINE: &str = "Advice (session model gpt-test): all workspaces · new · durable";

    /// Queues one pending candidate in `scope`, with the stored form of
    /// [`LINE`]'s advice when `advised`.
    pub fn candidate(service: &Service, scope: Scope, advised: bool) -> Candidate {
        let mut request = ProposeRequest {
            scope,
            kind: "preference".into(),
            key: "editor".into(),
            text: "private candidate body".into(),
            reason: "private reason".into(),
            source: super::super::Provenance {
                origin: Some(super::super::Origin::Model),
                ..super::super::Provenance::default()
            },
            ..ProposeRequest::default()
        };
        if advised {
            for (key, value) in [
                ("advice.model", "gpt-test"),
                ("advice.auxiliary", "false"),
                ("advice.scope", "user"),
                ("advice.relation", "new"),
                ("advice.durable", "true"),
            ] {
                request.metadata.insert(key.into(), value.into());
            }
        }
        service
            .propose(&request)
            .expect("proposal")
            .pop()
            .expect("candidate")
    }
}
