//! Tests for the lifecycle query over a fake neutral port service.

use super::selected_memory_tests::{RecallOnly, context};
use super::*;
use otto_core::agent::memory::{MemoryRecall, RecordStatus};
use otto_core::{memory as port, operation::OperationControl};

type InspectRecord = (String, String, String, Vec<(String, String)>);

/// Answers `inspect` Get from a table of (scope id, record id) to
/// (revision, metadata), `Conflict` for an absent record, and counts calls.
struct InspectTable {
    records: Vec<InspectRecord>,
    failing_id: Option<String>,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl port::MemoryService for InspectTable {
    fn describe(&self) -> port::ServiceDescription {
        let mut description = port::ServiceDescription::new("table-service".into());
        description
            .capabilities
            .push(port::Capability::new(port::Operation::Recall));
        description
            .capabilities
            .push(port::Capability::new(port::Operation::Inspect));
        description
    }
    async fn recall(
        &self,
        _: &port::RecallRequest,
        _: &dyn OperationControl,
    ) -> port::Result<port::RecallResult> {
        Err(port::Error::new(port::ErrorKind::Unsupported))
    }
    async fn inspect(
        &self,
        request: &port::InspectRequest,
        _: &dyn OperationControl,
    ) -> port::Result<port::InspectResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let port::InspectQuery::Get(reference) = &request.query else {
            return Err(port::Error::new(port::ErrorKind::Unsupported));
        };
        if self.failing_id.as_deref() == Some(reference.id.as_str()) {
            return Err(port::Error::new(port::ErrorKind::Unavailable));
        }
        let found = self
            .records
            .iter()
            .find(|(scope, id, _, _)| *scope == reference.scope.id && *id == reference.id);
        let Some((_, _, revision, metadata)) = found else {
            return Err(port::Error::new(port::ErrorKind::Conflict));
        };
        Ok(port::InspectResult {
            items: vec![port::RecallItem {
                reference: Some(port::MemoryRef {
                    revision: Some(revision.clone()),
                    ..reference.clone()
                }),
                content: port::Content::Text("record text".into()),
                sources: vec![],
                effective_at: None,
                metadata: metadata.iter().cloned().collect(),
            }],
            next_cursor: String::new(),
            state: "ready".into(),
        })
    }
}

fn entry(
    scope: &str,
    id: &str,
    revision: &str,
    metadata: &[(&str, &str)],
) -> (String, String, String, Vec<(String, String)>) {
    let metadata = metadata
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    (scope.into(), id.into(), revision.into(), metadata)
}

fn binding_over(table: InspectTable) -> (Arc<InspectTable>, ServiceBinding) {
    let table = Arc::new(table);
    let client = Arc::new(crate::memory::client::Client::new(table.clone()).unwrap());
    let binding = ServiceBinding {
        client,
        context: context(),
        observes: false,
        closed: AtomicBool::new(false),
    };
    (table, binding)
}

fn table(records: Vec<InspectRecord>) -> InspectTable {
    InspectTable {
        records,
        failing_id: None,
        calls: Default::default(),
    }
}

async fn lifecycles(
    binding: &ServiceBinding,
    ids: &[&str],
) -> Vec<otto_core::agent::memory::RecordLifecycle> {
    let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    binding
        .lifecycle(&ids, &CancellationToken::new())
        .await
        .unwrap()
}

/// Fails when an `otto.state` value maps to the wrong status, when the
/// replacement id is not read from `otto.superseded_by`, when a missing state
/// is not `Active`, or when the revision is not taken from the reference.
#[tokio::test]
async fn port_lifecycle_maps_each_otto_state_to_a_record_status() {
    let (_, binding) = binding_over(table(vec![
        entry("host-user", "a", "1", &[("otto.state", "active")]),
        entry("host-user", "b", "2", &[("otto.state", "needs_review")]),
        entry(
            "host-user",
            "c",
            "3",
            &[("otto.state", "superseded"), ("otto.superseded_by", "z")],
        ),
        entry("host-user", "d", "4", &[("otto.state", "retracted")]),
        entry("host-user", "e", "5", &[]),
        entry("host-user", "f", "6", &[("otto.state", "superseded")]),
    ]));

    let got = lifecycles(&binding, &["a", "b", "c", "d", "e", "f"]).await;

    let summary: Vec<(&str, Option<&str>, RecordStatus)> = got
        .iter()
        .map(|entry| {
            (
                entry.id.as_str(),
                entry.revision.as_deref(),
                entry.status.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("a", Some("1"), RecordStatus::Active),
            ("b", Some("2"), RecordStatus::NeedsReview),
            (
                "c",
                Some("3"),
                RecordStatus::Superseded {
                    replacement_id: "z".into()
                }
            ),
            ("d", Some("4"), RecordStatus::Retracted),
            ("e", Some("5"), RecordStatus::Active),
            (
                "f",
                Some("6"),
                RecordStatus::Superseded {
                    replacement_id: String::new()
                }
            ),
        ]
    );
}

/// Fails when an absent record is an error or has a status other than
/// `Unknown`, or when the second scope is not searched after the first.
#[tokio::test]
async fn port_lifecycle_searches_every_scope_and_reports_an_absent_record_as_unknown() {
    let (table, binding) = binding_over(table(vec![entry(
        "host-workspace",
        "in-workspace",
        "7",
        &[("otto.state", "retracted")],
    )]));

    let got = lifecycles(&binding, &["in-workspace", "absent"]).await;

    assert_eq!(got[0].status, RecordStatus::Retracted);
    assert_eq!(got[0].revision.as_deref(), Some("7"));
    assert_eq!(got[1].status, RecordStatus::Unknown);
    assert_eq!(got[1].revision, None);
    // in-workspace: user (absent) then workspace; absent: both scopes.
    assert_eq!(table.calls.load(Ordering::SeqCst), 4);
}

/// Fails when a service failure other than not-found is swallowed.
#[tokio::test]
async fn port_lifecycle_fails_the_call_on_a_service_error() {
    let mut failing = table(vec![entry("host-user", "ok", "1", &[])]);
    failing.failing_id = Some("broken".into());
    let (_, binding) = binding_over(failing);

    let ids = vec!["ok".to_string(), "broken".to_string()];
    let error = binding
        .lifecycle(&ids, &CancellationToken::new())
        .await
        .expect_err("service error");

    assert_eq!(
        error.0,
        port::Error::new(port::ErrorKind::Unavailable).to_string()
    );
}

/// Fails when a recall-only service (no Inspect operation) makes the call fail
/// instead of answering `Unknown` for every id.
#[tokio::test]
async fn port_lifecycle_answers_unknown_when_the_service_has_no_inspect() {
    let client =
        Arc::new(crate::memory::client::Client::new(Arc::new(RecallOnly::default())).unwrap());
    let binding = ServiceBinding {
        client,
        context: context(),
        observes: false,
        closed: AtomicBool::new(false),
    };

    let got = lifecycles(&binding, &["a", "b"]).await;

    assert!(
        got.iter()
            .all(|entry| entry.status == RecordStatus::Unknown)
    );
    assert_eq!(got.len(), 2);
}

/// Fails when a closed binding still queries the service.
#[tokio::test]
async fn port_lifecycle_on_a_closed_binding_fails_with_the_closed_error() {
    let (table, binding) = binding_over(table(vec![entry("host-user", "a", "1", &[])]));
    binding.closed.store(true, Ordering::SeqCst);

    let error = binding
        .lifecycle(&["a".to_string()], &CancellationToken::new())
        .await
        .expect_err("closed");

    assert_eq!(
        error.0,
        port::Error::new(port::ErrorKind::Closed).to_string()
    );
    assert_eq!(table.calls.load(Ordering::SeqCst), 0);
}
