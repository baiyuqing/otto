//! Tests for the lifecycle query of the agent seam.

use super::*;
use core_memory::{MemoryRecall, RecordStatus};

fn bound(service: &Arc<Service>, scopes: Vec<Scope>) -> Binding {
    service
        .bind(BindOptions {
            default_write_scope: scopes[0].clone(),
            scopes,
            ..BindOptions::default()
        })
        .expect("bind")
}

fn reference(scope: &Scope, id: &str) -> RecordRef {
    RecordRef {
        scope: scope.clone(),
        id: id.into(),
    }
}

fn change_state(
    service: &Service,
    record: &Record,
    state: RecordState,
    superseded_by: &str,
) -> Record {
    service
        .set_state(&SetStateRequest {
            reference: reference(&record.scope, &record.id),
            expected_revision: record.revision,
            state,
            superseded_by: superseded_by.into(),
            reason: String::new(),
        })
        .expect("set state")
}

async fn lifecycles(binding: &Binding, ids: &[&str]) -> Vec<core_memory::RecordLifecycle> {
    let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    binding
        .lifecycle(&ids, &CancellationToken::new())
        .await
        .expect("lifecycle")
}

/// Fails when a state maps to the wrong status, when the revision is not the
/// record's current revision, or when the replacement id is lost.
#[tokio::test]
async fn lifecycle_reports_each_record_state_with_its_current_revision() {
    let (_directory, service) = service();
    let scope = user_scope();
    let active = service.remember(&remember(&scope, "a", "active")).unwrap();
    let review = service.remember(&remember(&scope, "b", "review")).unwrap();
    let old = service.remember(&remember(&scope, "c", "old")).unwrap();
    let wrong = service.remember(&remember(&scope, "d", "wrong")).unwrap();
    let review = change_state(&service, &review, RecordState::NeedsReview, "");
    let old = change_state(&service, &old, RecordState::Superseded, &active.id);
    let wrong = change_state(&service, &wrong, RecordState::Retracted, "");
    let binding = bound(&service, vec![scope]);

    let ids = [&wrong.id, &active.id, &old.id, &review.id];
    let got = lifecycles(&binding, &ids.map(String::as_str)).await;

    let by_id = |id: &str| got.iter().find(|entry| entry.id == id).expect("entry");
    assert_eq!(by_id(&active.id).status, RecordStatus::Active);
    assert_eq!(by_id(&active.id).revision.as_deref(), Some("1"));
    assert_eq!(by_id(&review.id).status, RecordStatus::NeedsReview);
    assert_eq!(by_id(&review.id).revision.as_deref(), Some("2"));
    assert_eq!(
        by_id(&old.id).status,
        RecordStatus::Superseded {
            replacement_id: active.id.clone()
        }
    );
    assert_eq!(by_id(&old.id).revision.as_deref(), Some("2"));
    assert_eq!(by_id(&wrong.id).status, RecordStatus::Retracted);
    assert_eq!(got.len(), 4);
}

/// Fails when an active record past `expires_at` is reported `Active`.
#[tokio::test]
async fn lifecycle_reports_an_expired_record_as_expired() {
    let (_directory, service) = service();
    let scope = user_scope();
    let mut request = remember(&scope, "k", "short lived");
    request.expires_at = Some(Utc::now() + chrono::Duration::days(1));
    let record = service.remember(&request).unwrap();
    let now_binding = bound(&service, vec![scope.clone()]);
    let later_binding = service
        .bind(BindOptions {
            scopes: vec![scope.clone()],
            default_write_scope: scope,
            now: Some(|| Utc::now() + chrono::Duration::days(30)),
            ..BindOptions::default()
        })
        .unwrap();

    assert_eq!(
        lifecycles(&now_binding, &[&record.id]).await[0].status,
        RecordStatus::Active
    );
    let later = lifecycles(&later_binding, &[&record.id]).await;
    assert_eq!(later[0].status, RecordStatus::Expired);
    assert_eq!(later[0].revision.as_deref(), Some("1"));
}

/// Fails when a forgotten record is reported `Unknown` or loses the tombstone
/// revision, or when a never-stored id is not `Unknown` with no revision.
#[tokio::test]
async fn lifecycle_reports_a_forgotten_record_and_an_unknown_id() {
    let (_directory, service) = service();
    let scope = user_scope();
    let record = service
        .remember(&remember(&scope, "k", "to forget"))
        .unwrap();
    let outcome = service
        .forget(&ForgetRequest {
            reference: reference(&scope, &record.id),
            expected_revision: record.revision,
            purge_backups: false,
            confirm_purge: false,
        })
        .expect("forget");
    let tombstone = service
        .get_tombstone(&reference(&scope, &record.id))
        .expect("tombstone");
    assert!(!outcome.cleanup.is_empty());
    let binding = bound(&service, vec![scope]);

    let got = lifecycles(&binding, &["never-stored", &record.id]).await;

    let forgotten = got.iter().find(|entry| entry.id == record.id).unwrap();
    assert_eq!(forgotten.status, RecordStatus::Forgotten);
    assert_eq!(forgotten.revision, Some(tombstone.revision.to_string()));
    let unknown = got.iter().find(|entry| entry.id == "never-stored").unwrap();
    assert_eq!(unknown.status, RecordStatus::Unknown);
    assert_eq!(unknown.revision, None);
}

/// Fails when the lookup stops at the first scope.
#[tokio::test]
async fn lifecycle_finds_a_record_in_the_second_scope() {
    let (_directory, service) = service();
    let user = user_scope();
    let workspace = Scope::new(NAMESPACE_WORKSPACE, "ws-1");
    let in_workspace = service
        .remember(&remember(&workspace, "k", "workspace fact"))
        .unwrap();
    let in_user = service
        .remember(&remember(&user, "k", "user fact"))
        .unwrap();
    let in_workspace = change_state(&service, &in_workspace, RecordState::Retracted, "");
    let binding = bound(&service, vec![user, workspace]);

    let got = lifecycles(&binding, &[&in_workspace.id, &in_user.id]).await;

    assert_eq!(got[0].status, RecordStatus::Retracted);
    assert_eq!(got[1].status, RecordStatus::Active);
}

/// Fails when a closed binding answers instead of returning the closed error.
#[tokio::test]
async fn lifecycle_on_a_closed_binding_fails_like_recall() {
    let (_directory, service) = service();
    let scope = user_scope();
    let record = service.remember(&remember(&scope, "k", "v")).unwrap();
    let binding = bound(&service, vec![scope]);
    binding.release();

    let error = binding
        .lifecycle(&[record.id], &CancellationToken::new())
        .await
        .expect_err("closed");

    assert_eq!(error.0, ErrorKind::Closed.message());
}

/// Fails when a null service errors or reports a status other than `Unknown`.
#[tokio::test]
async fn lifecycle_on_a_null_service_answers_unknown_for_every_id() {
    let service = Arc::new(Service::null(None));
    let binding = bound(&service, vec![user_scope()]);

    let got = lifecycles(&binding, &["a", "b"]).await;

    assert_eq!(got.len(), 2);
    assert!(
        got.iter()
            .all(|entry| entry.status == RecordStatus::Unknown)
    );
    assert!(got.iter().all(|entry| entry.revision.is_none()));
}
