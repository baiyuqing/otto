use super::*;
use crate::memory::files::testsupport::open_temp;
use crate::memory::{CandidateRef, NAMESPACE_USER, NAMESPACE_WORKSPACE, Provenance};

fn service() -> (tempfile::TempDir, Arc<Service>) {
    let (directory, store) = open_temp();
    (
        directory,
        Arc::new(Service::new(store, decide_default_policy)),
    )
}

struct AlternateStore(crate::memory::files::FileStore);

fn unsupported<T>() -> Result<T> {
    Err(Error::new(ErrorKind::Unsupported))
}

impl MemoryStore for AlternateStore {
    fn identity(&self) -> Result<crate::memory::StoreIdentity> {
        self.0.identity()
    }
    fn close(&self) -> Result<()> {
        self.0.close()
    }
    fn get(&self, reference: &RecordRef) -> Result<Record> {
        self.0.get(reference)
    }
    fn get_by_key(&self, _: &RecordKey) -> Result<Record> {
        unsupported()
    }
    fn list(&self, _: &ListRequest) -> Result<super::super::RecordPage> {
        unsupported()
    }
    fn get_tombstone(&self, _: &RecordRef) -> Result<Tombstone> {
        unsupported()
    }
    fn get_candidate(&self, _: &super::super::CandidateRef) -> Result<Candidate> {
        unsupported()
    }
    fn list_candidates(&self, _: &CandidateListRequest) -> Result<super::super::CandidatePage> {
        unsupported()
    }
    fn retrieve(&self, _: &RetrievalRequest) -> Result<super::super::RetrievalResult> {
        unsupported()
    }
    fn upsert_within_standing_budget(&self, _: &UpsertRequest) -> Result<Record> {
        unsupported()
    }
    fn standing_usage(&self, _: &Scope, _: DateTime<Utc>) -> Result<StandingUsage> {
        unsupported()
    }
    fn standing_records(&self, _: &Scope, _: DateTime<Utc>) -> Result<Vec<Record>> {
        unsupported()
    }
    fn forget(&self, _: &StoreForgetRequest) -> Result<crate::memory::ForgetOutcome> {
        unsupported()
    }
    fn history(&self, _: &RecordRef) -> Result<RecordHistory> {
        unsupported()
    }
    fn restore(&self, _: &StoreRestoreRequest) -> Result<Record> {
        unsupported()
    }
    fn review_within_standing_budget(&self, _: &StoreReviewRequest) -> Result<ReviewResult> {
        unsupported()
    }
    fn propose(&self, _: &[Candidate]) -> Result<Vec<Candidate>> {
        unsupported()
    }
    fn check_proposals(&self, _: &[Candidate]) -> Result<()> {
        unsupported()
    }
}

#[test]
fn service_uses_injected_store_and_preserves_not_found_error() {
    let (_directory, store) = open_temp();
    let service = Service::from_store(Arc::new(AlternateStore(store)), decide_default_policy);

    let error = service
        .get(&RecordRef {
            scope: user_scope(),
            id: "missing".into(),
        })
        .expect_err("missing record");
    assert!(error.is(ErrorKind::NotFound));
}

fn user_scope() -> Scope {
    Scope::new(NAMESPACE_USER, "user-1")
}

fn remember(scope: &Scope, key: &str, text: &str) -> RememberRequest {
    RememberRequest {
        scope: scope.clone(),
        kind: "preference".into(),
        key: key.into(),
        text: text.into(),
        ..RememberRequest::default()
    }
}

fn model_proposal(scope: &Scope, key: &str, text: &str) -> ProposeRequest {
    ProposeRequest {
        action: CandidateAction::Create,
        scope: scope.clone(),
        kind: "preference".into(),
        key: key.into(),
        text: text.into(),
        source: Provenance {
            origin: Some(Origin::Model),
            ..Provenance::default()
        },
        ..ProposeRequest::default()
    }
}

mod binding;
mod history;
mod recall;
mod records;
