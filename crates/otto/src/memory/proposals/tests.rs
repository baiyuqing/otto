//! Tests of the memory proposal generator over a scripted backend and a real
//! temporary store.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::*;
use crate::decision::{Answer, NO};
use crate::memory::advice::audit::AuditStore;
use crate::memory::files::testsupport::open_temp;
use crate::memory::{
    CandidateListRequest, CandidateState, NAMESPACE_WORKSPACE, RememberRequest, Scope,
    decide_default_policy,
};

mod audit_and_state;
mod boundary;
mod fixtures;
mod layer_one;

pub(super) const RECORD_TEXT: &str = "Prefers the vim editor for every project";

/// One call the fake backend received.
#[derive(Debug, Clone)]
pub(super) struct Asked {
    pub state: String,
    pub questions: Vec<Question>,
    pub limit: Duration,
}

pub(super) type Round = std::result::Result<HashMap<String, Answer>, Failure>;

/// A backend that returns one scripted result per call, in order, and records
/// every call. An exhausted script answers with `Failure::Error`.
pub(super) struct FakeBackend {
    rounds: Mutex<VecDeque<Round>>,
    pub asked: Arc<Mutex<Vec<Asked>>>,
    model_name: String,
    auxiliary_model: bool,
    hang: bool,
}

impl FakeBackend {
    pub fn new(rounds: Vec<Round>) -> Self {
        Self {
            rounds: Mutex::new(rounds.into()),
            asked: Arc::default(),
            model_name: "gpt-test".into(),
            auxiliary_model: false,
            hang: false,
        }
    }

    pub fn auxiliary(mut self) -> Self {
        self.auxiliary_model = true;
        self
    }

    /// A backend whose `ask` never returns.
    pub fn hanging() -> Self {
        let mut backend = Self::new(Vec::new());
        backend.hang = true;
        backend
    }
}

#[async_trait::async_trait]
impl Backend for FakeBackend {
    fn model(&self) -> String {
        self.model_name.clone()
    }

    fn auxiliary(&self) -> bool {
        self.auxiliary_model
    }

    async fn ask(&self, state: &str, questions: &[Question], limit: Duration) -> Exchange {
        let call_number = {
            let mut asked = self.asked.lock().expect("asked");
            asked.push(Asked {
                state: state.to_owned(),
                questions: questions.to_vec(),
                limit,
            });
            asked.len()
        };
        if self.hang {
            std::future::pending::<()>().await;
        }
        let result = self
            .rounds
            .lock()
            .expect("rounds")
            .pop_front()
            .unwrap_or_else(|| Err(Failure::Error("no scripted round".into())));
        Exchange {
            request: format!("request {call_number}"),
            response: result.is_ok().then(|| format!("response {call_number}")),
            result,
        }
    }
}

/// Collects audit rows in memory.
#[derive(Default)]
pub(super) struct CollectingAudit {
    pub rows: Mutex<Vec<NewRow>>,
}

impl AuditStore for CollectingAudit {
    fn append(&self, row: &NewRow, _candidate_id: Option<&str>) -> audit::Result<()> {
        self.rows.lock().expect("rows").push(row.clone());
        Ok(())
    }
}

pub(super) struct Fixture {
    _directory: tempfile::TempDir,
    pub service: Service,
    pub target: Record,
}

/// A service over a temporary store holding one workspace record.
pub(super) fn fixture_with_text(text: &str) -> Fixture {
    let (directory, store) = open_temp();
    let service = Service::new(store, decide_default_policy);
    let target = service
        .remember(&RememberRequest {
            scope: Scope::new(NAMESPACE_WORKSPACE, "ws-1"),
            kind: "preference".into(),
            key: "editor".into(),
            text: text.into(),
            labels: vec!["tooling".into()],
            confidence: 0.8,
            ..RememberRequest::default()
        })
        .expect("remember target");
    Fixture {
        _directory: directory,
        service,
        target,
    }
}

pub(super) fn fixture() -> Fixture {
    fixture_with_text(RECORD_TEXT)
}

pub(super) fn evidence(id: &str, text: &str, trust: Trust) -> EvidenceItem {
    EvidenceItem {
        id: id.into(),
        text: text.into(),
        trust,
        source: "user".into(),
    }
}

pub(super) fn input(
    target: &Record,
    evidence: Vec<EvidenceItem>,
    permitted: &[ProposalAction],
) -> ProposalInput {
    ProposalInput {
        target: target.clone(),
        evidence,
        permitted: permitted.to_vec(),
        session_id: "session-1".into(),
    }
}

pub(super) fn all_actions() -> Vec<ProposalAction> {
    ProposalAction::ALL.to_vec()
}

pub(super) fn answer(choice: &str) -> Answer {
    Answer {
        choice: choice.into(),
        probability: None,
    }
}

/// Round one: the action, then one yes/no per entry of `supports`.
pub(super) fn round_one(action: &str, supports: &[bool]) -> Round {
    let mut answers = HashMap::from([(ACTION_QUESTION.to_owned(), answer(action))]);
    for (index, supported) in supports.iter().enumerate() {
        answers.insert(
            EVIDENCE_QUESTIONS[index].to_owned(),
            answer(if *supported { YES } else { NO }),
        );
    }
    Ok(answers)
}

/// Round two: the explanation and an optional replacement.
pub(super) fn round_two(explanation: &str, replacement: Option<&str>) -> Round {
    let mut answers = HashMap::from([(EXPLANATION_QUESTION.to_owned(), answer(explanation))]);
    if let Some(replacement) = replacement {
        answers.insert(REPLACEMENT_QUESTION.to_owned(), answer(replacement));
    }
    Ok(answers)
}

pub(super) fn generator(rounds: Vec<Round>) -> (Generator, Arc<Mutex<Vec<Asked>>>) {
    let backend = FakeBackend::new(rounds);
    let asked = Arc::clone(&backend.asked);
    (Generator::new(Box::new(backend), None), asked)
}

pub(super) async fn run(generator: &Generator, fixture: &Fixture, input: ProposalInput) -> Outcome {
    generator
        .propose(&fixture.service, input, &CancellationToken::new())
        .await
}

pub(super) fn pending_candidates(service: &Service) -> Vec<Candidate> {
    service
        .list_candidates(&CandidateListRequest {
            scopes: vec![Scope::new(NAMESPACE_WORKSPACE, "ws-1")],
            states: vec![CandidateState::Pending],
            limit: 50,
            cursor: String::new(),
        })
        .expect("list candidates")
        .candidates
}

pub(super) fn local_evidence() -> Vec<EvidenceItem> {
    vec![evidence(
        "e1",
        "I stopped using vim last month.",
        Trust::Local,
    )]
}

pub(super) fn proposed(outcome: Outcome) -> Candidate {
    match outcome {
        Outcome::Proposed(candidate) => candidate,
        other => panic!("expected Outcome::Proposed, got {other:?}"),
    }
}

pub(super) fn rejected(outcome: Outcome) -> String {
    match outcome {
        Outcome::Rejected(reason) => reason,
        other => panic!("expected Outcome::Rejected, got {other:?}"),
    }
}
