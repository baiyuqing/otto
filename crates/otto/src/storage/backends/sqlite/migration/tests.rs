use super::*;
use crate::approval::{Actor, Proposal, Service};
use otto_core::approval::ApprovalMode;
use otto_core::approval::{ApplicationState, Authority, Kind, Scope, Subject};
use std::sync::atomic::{AtomicBool, Ordering};

struct Fence(AtomicBool);
impl FreezeGuard for Fence {
    fn validate(&self, generation: &str) -> Result<()> {
        if self.0.load(Ordering::SeqCst) && generation == "frozen-1" {
            Ok(())
        } else {
            Err(Error::Conflict("maintenance marker changed".into()))
        }
    }
}
#[test]
fn approval_schema_accepts_its_native_sequence_and_rejects_unknown_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approvals.db");
    Service::open(&path).unwrap();
    let connection = Connection::open(&path).unwrap();
    assert!(ApprovalCodec.validate(&connection).is_ok());
    connection
        .execute_batch("CREATE TABLE unowned_approval_state(id TEXT)")
        .unwrap();
    assert!(matches!(
        ApprovalCodec.validate(&connection),
        Err(Error::Invalid(_))
    ));
}
#[tokio::test]
async fn approval_export_import_resumes_receipts_preserves_unknown_fields_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.db");
    let stage_path = dir.path().join("destination.db");
    let service = Service::open(&path).unwrap();
    let scope = Scope {
        workspace: "/w".into(),
        session_id: "s".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    let request = service
        .create(Proposal {
            kind: Kind::Authorization,
            authority: Authority::Durable,
            scope: scope.clone(),
            subject: Subject {
                tool_call_id: String::new(),
                source: "native-review".into(),
                id: "operation".into(),
                version: "7".into(),
                details: "prepared".into(),
            },
            title: "Review".into(),
            reason: "scope".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    service
        .decide_and_claim(&request.id, &scope, 1, true, &Actor::local("test"))
        .unwrap();
    drop(service);
    let connection = Connection::open(&path).unwrap();
    let body: String = connection
        .query_row(
            "SELECT body FROM approvals WHERE id=?1",
            [request.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    fn raw_extension(body: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        let pretty = serde_json::to_string_pretty(&value).unwrap();
        let prefix = pretty.strip_suffix('}').unwrap();
        format!(
            "\n\t{prefix},\n  \"future_extension\": {{\"receipt\": \"keep-me\", \"precise\": 123456789012345678901234567890.123456789012345678901234567890}}\n}} \r\n"
        )
    }
    let original_body = raw_extension(&body);
    connection
        .execute(
            "UPDATE approvals SET body=?2 WHERE id=?1",
            params![request.id, original_body],
        )
        .unwrap();
    let mut statement = connection
        .prepare("SELECT sequence,body FROM approval_events ORDER BY sequence")
        .unwrap();
    let event_bodies = statement
        .query_map((), |r| Ok((r.get::<i64>(0)?, r.get::<String>(1)?)))
        .unwrap()
        .collect::<crate::storage::Result<Vec<_>>>()
        .unwrap()
        .into_iter()
        .map(|(sequence, body)| (sequence, raw_extension(&body)))
        .collect::<Vec<_>>();
    drop(statement);
    for (sequence, body) in &event_bodies {
        connection
            .execute(
                "UPDATE approval_events SET body=?2 WHERE sequence=?1",
                params![sequence, body],
            )
            .unwrap();
    }
    connection.close().unwrap();
    let fence = Arc::new(Fence(AtomicBool::new(true)));
    let codec = Arc::new(ApprovalCodec);
    let source = Source::open(
        &path,
        "source".into(),
        "frozen-1".into(),
        fence.clone(),
        codec.clone(),
    )
    .unwrap();
    let snapshot = source.descriptor().await.unwrap();
    assert_eq!(snapshot.domains["approvals"].committed_cursor, "2");
    let mut manifest = Manifest {
        format_version: FORMAT_VERSION,
        migration_id: "copy".into(),
        destination_identity: "target".into(),
        expected_route_generation: "route-1".into(),
        max_chunk_bytes: 64 * 1024,
        source: snapshot.clone(),
        progress: BTreeMap::from([(
            "approvals".into(),
            Progress {
                cursor: String::new(),
                complete: false,
                chunks: vec![],
            },
        )]),
        destination_write_started: true,
        sealed: false,
        published_generation: None,
        rolled_back: false,
    };
    let stage = Stage::new(stage_path.clone(), "target".into(), codec.clone());
    stage.begin(&manifest).await.unwrap();
    let chunk = source.read_chunk("approvals", "", 64 * 1024).await.unwrap();
    assert!(chunk.next_cursor.is_none());
    let hash = chunk_hash(&chunk, 64 * 1024).unwrap();
    let receipt = stage
        .apply_chunk("copy", "approvals", &chunk, &hash)
        .await
        .unwrap();
    stage.close().await.unwrap();
    assert!(matches!(
        stage.begin(&manifest).await,
        Err(Error::Conflict(_))
    ));
    // Simulate a lost coordinator checkpoint after the destination commit.
    let stage = Stage::new(stage_path.clone(), "target".into(), codec);
    stage.begin(&manifest).await.unwrap();
    assert_eq!(
        stage.chunk_receipt("copy", "approvals", "").await.unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(
        stage
            .apply_chunk("copy", "approvals", &chunk, &hash)
            .await
            .unwrap(),
        receipt
    );
    manifest
        .progress
        .get_mut("approvals")
        .unwrap()
        .chunks
        .push(receipt);
    manifest.progress.get_mut("approvals").unwrap().complete = true;
    assert_eq!(
        stage.verify(&manifest).await.unwrap()["approvals"],
        snapshot.domains["approvals"].digest
    );
    stage.seal(&manifest).await.unwrap();
    stage.seal(&manifest).await.unwrap();
    stage.close().await.unwrap();
    let connection = Connection::open(&stage_path).unwrap();
    let migrated: String = connection
        .query_row(
            "SELECT body FROM approvals WHERE id=?1",
            [request.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(migrated, original_body);
    let mut statement = connection
        .prepare("SELECT sequence,body FROM approval_events ORDER BY sequence")
        .unwrap();
    let migrated_events = statement
        .query_map((), |r| Ok((r.get::<i64>(0)?, r.get::<String>(1)?)))
        .unwrap()
        .collect::<crate::storage::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(migrated_events, event_bodies);
    drop(statement);
    connection.close().unwrap();
    let service = Service::open(&stage_path).unwrap();
    assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
    assert_eq!(service.events("/w", Some("s"), 0).unwrap().len(), 2);
    service.apply_available("/w").unwrap();
    assert_eq!(
        service
            .get(&request.id, "/w", Some("s"))
            .unwrap()
            .application,
        ApplicationState::Unknown
    );
    let changed = Connection::open(&stage_path)
        .unwrap()
        .query_row(
            "SELECT body FROM approvals WHERE id=?1",
            [request.id.as_str()],
            |row| row.get::<String>(0),
        )
        .unwrap();
    let raw_fields = |body: &str| {
        serde_json::from_str::<BTreeMap<String, Box<serde_json::value::RawValue>>>(body).unwrap()
    };
    assert_eq!(
        raw_fields(&changed)["future_extension"].get(),
        raw_fields(&original_body)["future_extension"].get()
    );
    let event = Connection::open(&stage_path)
        .unwrap()
        .query_row(
            "SELECT body FROM approval_events ORDER BY sequence DESC LIMIT 1",
            (),
            |row| row.get::<String>(0),
        )
        .unwrap();
    assert_eq!(
        raw_fields(&event)["future_extension"].get(),
        raw_fields(&original_body)["future_extension"].get()
    );
    let mut future_schema: serde_json::Value = serde_json::from_str(&original_body).unwrap();
    future_schema["schema_version"] = serde_json::json!(2);
    assert!(approval_request(&serde_json::to_string(&future_schema).unwrap()).is_err());
    future_schema["schema_version"] = serde_json::json!(1);
    future_schema["scope"]["unrecognized_authority"] = serde_json::json!(true);
    assert!(approval_request(&serde_json::to_string(&future_schema).unwrap()).is_err());
    // Wire/model decoding remains strict even for the allowed persisted
    // additive envelope metadata.
    assert!(serde_json::from_str::<otto_core::approval::Request>(&original_body).is_err());
    // Explicitly imported IDs must advance the ordinary allocator too.
    let fresh = service
        .create(Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Consent,
            scope: scope.clone(),
            subject: Subject {
                tool_call_id: String::new(),
                source: "conversation".into(),
                id: "post-migration-plan".into(),
                version: "1".into(),
                details: "Review the new plan.".into(),
            },
            title: "Review new plan".into(),
            reason: "scope".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    service
        .decide(
            &fresh.id,
            &scope,
            fresh.revision,
            true,
            "",
            &Actor::local("test"),
        )
        .unwrap();
    let source_cursor = snapshot.domains["approvals"]
        .committed_cursor
        .parse::<i64>()
        .unwrap();
    let fresh_events = service
        .events("/w", Some("s"), source_cursor)
        .unwrap()
        .into_iter()
        .filter(|event| event.request.id == fresh.id)
        .collect::<Vec<_>>();
    assert_eq!(fresh_events.len(), 2);
    assert!(fresh_events[0].sequence > source_cursor);
    assert!(fresh_events[1].sequence > fresh_events[0].sequence);
    fence.0.store(false, Ordering::SeqCst);
    assert!(source.validate_frozen(&snapshot).await.is_err());
    assert!(source.close().await.is_err());
}

#[tokio::test]
async fn invalid_chunk_rolls_back_records_and_receipt_and_small_limit_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.db");
    let service = Service::open(&path).unwrap();
    let scope = Scope {
        workspace: "/w".into(),
        session_id: "s".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    drop(service);
    let codec = Arc::new(ApprovalCodec);
    let source = Source::open(
        &path,
        "source".into(),
        "frozen-1".into(),
        Arc::new(Fence(AtomicBool::new(true))),
        codec.clone(),
    )
    .unwrap();
    assert!(source.read_chunk("approvals", "", 32).await.is_err());
    let snapshot = source.descriptor().await.unwrap();
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        migration_id: "bad".into(),
        destination_identity: "target".into(),
        expected_route_generation: "route-1".into(),
        max_chunk_bytes: 64 * 1024,
        source: snapshot,
        progress: BTreeMap::from([(
            "approvals".into(),
            Progress {
                cursor: String::new(),
                complete: false,
                chunks: vec![],
            },
        )]),
        destination_write_started: true,
        sealed: false,
        published_generation: None,
        rolled_back: false,
    };
    let stage_path = dir.path().join("destination.db");
    let stage = Stage::new(stage_path.clone(), "target".into(), codec);
    stage.begin(&manifest).await.unwrap();
    let mut chunk = source.read_chunk("approvals", "", 64 * 1024).await.unwrap();
    chunk.records.push(Record {
        id: "invalid".into(),
        revision: "1".into(),
        payload: b"{}".to_vec(),
    });
    let hash = chunk_hash(&chunk, 64 * 1024).unwrap();
    assert!(
        stage
            .apply_chunk("bad", "approvals", &chunk, &hash)
            .await
            .is_err()
    );
    assert!(
        stage
            .chunk_receipt("bad", "approvals", "")
            .await
            .unwrap()
            .is_none()
    );
    let connection = Connection::open(&stage_path).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM approval_modes", (), |r| r
                .get::<i64>(0))
            .unwrap(),
        0
    );
    source.close().await.unwrap();
}
