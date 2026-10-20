use super::*;
use crate::storage::migration::group::{GroupSource, GroupStage, ReferenceClosure};

struct Fence;
impl FreezeGuard for Fence {
    fn validate(&self, generation: &str) -> Result<()> {
        if generation == "freeze-1" {
            Ok(())
        } else {
            Err(conflict("different freeze"))
        }
    }
}
struct Closure;
#[async_trait]
impl ReferenceClosure for Closure {
    async fn validate(&self, _: &Manifest) -> Result<()> {
        Ok(())
    }
}
struct MissingReference;
#[async_trait]
impl ReferenceClosure for MissingReference {
    async fn validate(&self, _: &Manifest) -> Result<()> {
        Err(invalid("missing cross-domain reference"))
    }
}

fn tempdir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}
fn journal(id: &str, parent: Option<&str>) -> Vec<u8> {
    let mut header = serde_json::json!({ "type":"session", "version":3, "id":id, "timestamp":"2026-10-06T00:00:00Z", "cwd":"/workspace", "futureHeader":{"keep":true} });
    if let Some(parent) = parent {
        header["parentSession"] = serde_json::json!(parent);
    }
    let entry = serde_json::json!({ "type":"future_entry", "id":"00000001", "parentId":null, "timestamp":"2026-10-06T00:00:01Z", "unknown":{"verbatim":"yes"} });
    format!("{header}\n{entry}\n").into_bytes()
}
fn source(root: &Path, domain: Domain, inputs: Vec<JournalInput>) -> JournalSource {
    JournalSource::open(
        root,
        domain,
        inputs,
        "source".into(),
        "freeze-1".into(),
        Arc::new(Fence),
    )
    .unwrap()
}
fn input(path: &str) -> JournalInput {
    JournalInput {
        relative_path: path.into(),
        legacy_path: None,
        artifact_id: None,
    }
}
async fn manifest(source: &dyn SnapshotExport) -> Manifest {
    let snapshot = source.descriptor().await.unwrap();
    Manifest {
        format_version: 1,
        max_chunk_bytes: 8192,
        migration_id: "move-1".into(),
        destination_identity: "destination".into(),
        expected_route_generation: "route-1".into(),
        progress: snapshot
            .domains
            .keys()
            .map(|domain| {
                (
                    domain.clone(),
                    Progress {
                        cursor: String::new(),
                        complete: false,
                        chunks: vec![],
                    },
                )
            })
            .collect(),
        source: snapshot,
        destination_write_started: true,
        sealed: false,
        published_generation: None,
        rolled_back: false,
    }
}
async fn copy(source: &dyn SnapshotExport, stage: &dyn StageImport, manifest: &mut Manifest) {
    stage.begin(manifest).await.unwrap();
    for domain in manifest.source.domains.keys().cloned().collect::<Vec<_>>() {
        let mut position = String::new();
        loop {
            let chunk = source
                .read_chunk(&domain, &position, manifest.max_chunk_bytes)
                .await
                .unwrap();
            let hash = chunk_hash(&chunk, manifest.max_chunk_bytes).unwrap();
            let receipt = stage
                .apply_chunk(&manifest.migration_id, &domain, &chunk, &hash)
                .await
                .unwrap();
            // Stable idempotency reuses the same atomic payload+receipt file.
            assert_eq!(
                stage
                    .apply_chunk(&manifest.migration_id, &domain, &chunk, &hash)
                    .await
                    .unwrap(),
                receipt
            );
            assert_eq!(
                stage
                    .chunk_receipt(&manifest.migration_id, &domain, &position)
                    .await
                    .unwrap(),
                Some(receipt.clone())
            );
            let progress = manifest.progress.get_mut(&domain).unwrap();
            progress.chunks.push(receipt);
            if let Some(next) = chunk.next_cursor {
                position = next;
                progress.cursor = position.clone();
            } else {
                progress.complete = true;
                break;
            }
        }
    }
}

#[tokio::test]
async fn legacy_public_session_root_private_workspace_transfers_without_permission_changes() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let from = tempdir();
    let to = tempdir();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(from.path().join("workspace"))
        .unwrap();
    let bytes = journal("session-id", None);
    std::fs::write(from.path().join("workspace/session.jsonl"), &bytes).unwrap();
    std::fs::set_permissions(from.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    super::super::session::Store::read_entries(from.path().join("workspace/session.jsonl"))
        .unwrap();
    let source = source(
        from.path(),
        Domain::Sessions,
        vec![input("workspace/session.jsonl")],
    );
    let mut manifest = manifest(&source).await;
    let stage = JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
    copy(&source, &stage, &mut manifest).await;
    stage.seal(&manifest).await.unwrap();
    assert_eq!(
        std::fs::read(to.path().join("workspace/session.jsonl")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::metadata(from.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(from.path().join("workspace"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(JournalStage::new(from.path(), Domain::Sessions, "destination".into()).is_err());
    assert!(super::super::migration::LocalJournal::open(from.path()).is_err());
    source.close().await.unwrap();
}

#[tokio::test]
async fn legacy_root_without_metadata_keeps_permissions_but_present_dataset_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempdir();
    std::fs::write(
        root.path().join("session.jsonl"),
        journal("session-id", None),
    )
    .unwrap();
    let source = source(root.path(), Domain::Sessions, vec![input("session.jsonl")]);
    let snapshot = source.descriptor().await.unwrap();
    let record = source.files.lock().unwrap().as_ref().unwrap()[0]
        .record
        .clone();
    let files = BTreeMap::from([(record.relative_path.clone(), record)]);
    let (aliases, session_ids) = mappings(&files, Domain::Sessions).unwrap();
    let dataset = Dataset {
        version: 1,
        migration_id: "move-1".into(),
        snapshot,
        files,
        aliases,
        session_ids,
    };
    source.close().await.unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        read_dataset_alias(root.path(), "/legacy/session.jsonl").unwrap(),
        None
    );
    assert_eq!(
        super::super::session::Store::read_entries(root.path().join("session.jsonl"))
            .unwrap()
            .0
            .len(),
        1
    );
    assert_eq!(
        std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
    std::fs::write(
        root.path().join(Domain::Sessions.dataset_name()),
        serde_json::to_vec(&dataset).unwrap(),
    )
    .unwrap();
    assert!(
        read_dataset_alias(root.path(), "/legacy/session.jsonl")
            .unwrap_err()
            .contains("mode 0700")
    );
    assert!(read_dataset_alias(&root.path().join(".."), "/legacy/session.jsonl").is_err());
}

#[tokio::test]
async fn exact_unknown_pi_bytes_parent_aliases_and_incomplete_tail_survive_reopen() {
    let from = tempdir();
    let to = tempdir();
    let parent_path = from
        .path()
        .join("parent.jsonl")
        .to_string_lossy()
        .into_owned();
    let parent_bytes = journal("parent-id", None);
    let mut child_bytes = journal("child-id", Some(&parent_path));
    child_bytes.extend_from_slice(b"{\"type\":\"message\"");
    std::fs::write(from.path().join("parent.jsonl"), &parent_bytes).unwrap();
    std::fs::write(from.path().join("child.jsonl"), &child_bytes).unwrap();
    let source = source(
        from.path(),
        Domain::Sessions,
        vec![input("parent.jsonl"), input("child.jsonl")],
    );
    let mut manifest = manifest(&source).await;
    let stage = JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
    copy(&source, &stage, &mut manifest).await;
    stage.verify(&manifest).await.unwrap();
    assert!(stage.dataset().await.is_err());
    let references = stage.reference_dataset(&manifest).await.unwrap();
    assert_eq!(references.aliases[&parent_path], "parent.jsonl");
    assert_eq!(references.session_ids["child-id"], "child.jsonl");
    stage.seal(&manifest).await.unwrap();
    assert_eq!(
        std::fs::read(to.path().join("parent.jsonl")).unwrap(),
        parent_bytes
    );
    assert_eq!(
        std::fs::read(to.path().join("child.jsonl")).unwrap(),
        child_bytes
    );
    assert_eq!(
        read_dataset_alias(to.path(), &parent_path).unwrap(),
        Some(PathBuf::from("parent.jsonl"))
    );
    stage.close().await.unwrap();
    assert!(stage.verify(&manifest).await.is_err());
    let reopened = JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
    reopened.verify(&manifest).await.unwrap();
    let (entries, _) =
        super::super::session::Store::read_entries(to.path().join("child.jsonl")).unwrap();
    assert_eq!(entries[0].type_name, "future_entry");
    source.close().await.unwrap();
    assert!(source.descriptor().await.is_err());
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(to.path().join("parent.jsonl"))
        .unwrap();
    append.write_all(b"{\"type\":\"future_entry\",\"id\":\"00000002\",\"parentId\":\"00000001\",\"timestamp\":\"2026-10-06T00:00:02Z\"}\n").unwrap();
    assert_eq!(
        read_dataset_alias(to.path(), &parent_path).unwrap(),
        Some(PathBuf::from("parent.jsonl"))
    );
    std::fs::write(
        to.path().join("parent.jsonl"),
        journal("different-session", None),
    )
    .unwrap();
    assert!(read_dataset_alias(to.path(), &parent_path).is_err());
}

#[tokio::test]
async fn atomic_chunk_survives_new_stage_instance_before_checkpoint_and_blob_stream_is_bounded() {
    let from = tempdir();
    let to = tempdir();
    std::fs::create_dir(from.path().join("artifacts")).unwrap();
    let bytes: Vec<_> = (0..100_000).map(|index| (index % 256) as u8).collect();
    std::fs::write(from.path().join("artifacts/output.bin"), &bytes).unwrap();
    let source = source(
        from.path(),
        Domain::Artifacts,
        vec![JournalInput {
            relative_path: "artifacts/output.bin".into(),
            legacy_path: None,
            artifact_id: Some("artifact-1".into()),
        }],
    );
    let mut manifest = manifest(&source).await;
    let first = JournalStage::new(to.path(), Domain::Artifacts, "destination".into()).unwrap();
    first.begin(&manifest).await.unwrap();
    let chunk = source.read_chunk("artifacts", "", 8192).await.unwrap();
    let hash = chunk_hash(&chunk, 8192).unwrap();
    first
        .apply_chunk("move-1", "artifacts", &chunk, &hash)
        .await
        .unwrap();
    first.close().await.unwrap();
    let reopened = JournalStage::new(to.path(), Domain::Artifacts, "destination".into()).unwrap();
    assert!(
        reopened
            .chunk_receipt("move-1", "artifacts", "")
            .await
            .unwrap()
            .is_some()
    );
    copy(&source, &reopened, &mut manifest).await;
    assert!(manifest.progress["artifacts"].chunks.len() > 2);
    reopened.verify(&manifest).await.unwrap();
    reopened.seal(&manifest).await.unwrap();
    assert_eq!(
        std::fs::read(to.path().join("artifacts/output.bin")).unwrap(),
        bytes
    );
    let last = source.read_chunk("artifacts", "0:0", 8192).await.unwrap();
    assert!(serde_json::to_vec(&last).unwrap().len() <= 8192);
    assert!(
        source
            .read_chunk("artifacts", &format!("{}:0", usize::MAX), 8192)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn owned_file_lock_symlink_escape_and_missing_parent_fail_closed() {
    use std::os::unix::fs::symlink;
    let root = tempdir();
    let outside = tempdir();
    std::fs::write(root.path().join("parent.jsonl"), journal("parent-id", None)).unwrap();
    let writer = fsops::open_no_follow(&root.path().join("parent.jsonl"), libc::O_RDWR).unwrap();
    posix::try_lock_exclusive(&writer).unwrap();
    assert!(
        JournalSource::open(
            root.path(),
            Domain::Sessions,
            vec![input("parent.jsonl")],
            "source".into(),
            "freeze-1".into(),
            Arc::new(Fence)
        )
        .is_err()
    );
    drop(writer);
    std::fs::write(
        outside.path().join("other.jsonl"),
        journal("other-id", None),
    )
    .unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    assert!(
        JournalSource::open(
            root.path(),
            Domain::Sessions,
            vec![input("escape/other.jsonl")],
            "source".into(),
            "freeze-1".into(),
            Arc::new(Fence)
        )
        .is_err()
    );
    assert!(
        JournalSource::open(
            root.path(),
            Domain::Sessions,
            vec![input("../other.jsonl")],
            "source".into(),
            "freeze-1".into(),
            Arc::new(Fence)
        )
        .is_err()
    );
    std::fs::write(
        root.path().join("child.jsonl"),
        journal("child-id", Some("/missing/parent.jsonl")),
    )
    .unwrap();
    assert!(
        JournalSource::open(
            root.path(),
            Domain::Sessions,
            vec![input("child.jsonl")],
            "source".into(),
            "freeze-1".into(),
            Arc::new(Fence)
        )
        .is_err()
    );
}

#[tokio::test]
async fn corrupted_receipts_or_blob_ranges_never_verify_or_seal() {
    let from = tempdir();
    let to = tempdir();
    std::fs::write(
        from.path().join("session.jsonl"),
        journal("session-id", None),
    )
    .unwrap();
    let source = source(from.path(), Domain::Sessions, vec![input("session.jsonl")]);
    let mut manifest = manifest(&source).await;
    let stage = JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap();
    copy(&source, &stage, &mut manifest).await;
    std::fs::write(to.path().join(chunk_name(Domain::Sessions, "")), b"corrupt").unwrap();
    assert!(stage.chunk_receipt("move-1", "sessions", "").await.is_err());
    assert!(stage.verify(&manifest).await.is_err());
    assert!(stage.seal(&manifest).await.is_err());
    assert!(stage.dataset().await.is_err());
}

#[tokio::test]
async fn explicit_abort_after_failed_closure_restores_unchanged_source_without_activating_stage() {
    use super::super::migration::{
        LocalCutover, LocalJournal, PreparedCutover, RouteGroup, StoppedOwners,
    };
    struct Stopped;
    impl StoppedOwners for Stopped {
        fn verify_stopped(&self) -> Result<()> {
            Ok(())
        }
    }
    let from = tempdir();
    let to = tempdir();
    let controls = tempdir();
    let bytes = journal("session-id", None);
    std::fs::write(from.path().join("session.jsonl"), &bytes).unwrap();
    let routes = |root: &Path| {
        BTreeMap::from([(
            "sessions".into(),
            StorageRoute {
                backend: "posix".into(),
                endpoint: root.to_string_lossy().into_owned(),
                namespace_identity: None,
            },
        )])
    };
    let source_routes = routes(from.path());
    let destination_routes = routes(to.path());
    let control = LocalCutover::open(
        controls.path(),
        RouteGroup {
            identity: "source".into(),
            generation: "route-1".into(),
            writer_epoch: "epoch-1".into(),
            domains: source_routes.clone(),
        },
    )
    .unwrap();
    control
        .freeze(
            PreparedCutover {
                migration_id: "move-1".into(),
                source_identity: "source".into(),
                destination_identity: "destination".into(),
                source_generation: "freeze-1".into(),
                expected_route_generation: "route-1".into(),
                source_routes,
                destination_routes,
            },
            Arc::new(Stopped),
        )
        .await
        .unwrap();
    let source = JournalSource::open(
        from.path(),
        Domain::Sessions,
        vec![input("session.jsonl")],
        "source".into(),
        "freeze-1".into(),
        Arc::new(control.clone()),
    )
    .unwrap();
    let mut manifest = manifest(&source).await;
    let journal = LocalJournal::open(controls.path()).unwrap();
    let revision = journal.save(None, &manifest).await.unwrap();
    let stage: Arc<dyn StageImport> =
        Arc::new(JournalStage::new(to.path(), Domain::Sessions, "destination".into()).unwrap());
    let group = GroupStage::new(
        BTreeMap::from([("sessions".into(), stage)]),
        Arc::new(MissingReference),
    )
    .unwrap();
    copy(&source, &group, &mut manifest).await;
    assert!(group.verify(&manifest).await.is_err());
    journal.save(Some(revision), &manifest).await.unwrap();
    source.close().await.unwrap();
    group.close().await.unwrap();
    control.abort_planned("move-1").await.unwrap();
    control.abort_planned("move-1").await.unwrap();
    control.ensure_writable("source", "route-1").await.unwrap();
    assert_eq!(
        std::fs::read(from.path().join("session.jsonl")).unwrap(),
        bytes
    );
    assert!(control.receipt("move-1").await.unwrap().is_none());
    assert!(!to.path().join(Domain::Sessions.dataset_name()).exists());
    let aborted = journal.load("move-1").await.unwrap().unwrap();
    assert!(aborted.manifest.rolled_back && aborted.manifest.destination_write_started);
    assert!(
        journal
            .save(Some(aborted.revision), &manifest)
            .await
            .is_err()
    );
    assert!(control.publish(&manifest).await.is_err());
}

#[tokio::test]
async fn group_composes_native_domains_and_rejects_unverified_cross_store_closure() {
    let session_root = tempdir();
    let artifact_root = tempdir();
    let session_to = tempdir();
    let artifact_to = tempdir();
    std::fs::write(
        session_root.path().join("session.jsonl"),
        journal("session-id", None),
    )
    .unwrap();
    std::fs::create_dir(artifact_root.path().join("artifacts")).unwrap();
    std::fs::write(artifact_root.path().join("artifacts/value"), b"artifact").unwrap();
    let sessions: Arc<dyn SnapshotExport> = Arc::new(source(
        session_root.path(),
        Domain::Sessions,
        vec![input("session.jsonl")],
    ));
    let artifacts: Arc<dyn SnapshotExport> = Arc::new(source(
        artifact_root.path(),
        Domain::Artifacts,
        vec![JournalInput {
            relative_path: "artifacts/value".into(),
            legacy_path: None,
            artifact_id: Some("artifact-id".into()),
        }],
    ));
    let source = GroupSource::new(vec![sessions, artifacts]).await.unwrap();
    let mut manifest = manifest(&source).await;
    let session_stage: Arc<dyn StageImport> = Arc::new(
        JournalStage::new(session_to.path(), Domain::Sessions, "destination".into()).unwrap(),
    );
    let artifact_stage: Arc<dyn StageImport> = Arc::new(
        JournalStage::new(artifact_to.path(), Domain::Artifacts, "destination".into()).unwrap(),
    );
    let destinations = BTreeMap::from([
        ("sessions".into(), session_stage.clone()),
        ("artifacts".into(), artifact_stage.clone()),
    ]);
    let missing = GroupStage::new(
        BTreeMap::from([("sessions".into(), session_stage.clone())]),
        Arc::new(Closure),
    )
    .unwrap();
    assert!(missing.validate_plan(&manifest).await.is_err());
    let mut wrong_schema = manifest.clone();
    wrong_schema
        .source
        .domains
        .get_mut("sessions")
        .unwrap()
        .schema_family = "unsupported".into();
    let rejected = GroupStage::new(destinations.clone(), Arc::new(MissingReference)).unwrap();
    assert!(rejected.validate_plan(&wrong_schema).await.is_err());
    copy(&source, &rejected, &mut manifest).await;
    assert!(rejected.verify(&manifest).await.is_err());
    assert!(rejected.seal(&manifest).await.is_err());
    assert!(
        !session_to
            .path()
            .join(Domain::Sessions.dataset_name())
            .exists()
    );
    let accepted = GroupStage::new(destinations, Arc::new(Closure)).unwrap();
    assert_eq!(accepted.verify(&manifest).await.unwrap().len(), 2);
    accepted.seal(&manifest).await.unwrap();
    accepted.close().await.unwrap();
    let mut scoped = manifest.clone();
    scoped
        .source
        .domains
        .retain(|domain, _| domain == "sessions");
    scoped.progress.retain(|domain, _| domain == "sessions");
    assert!(session_stage.verify(&scoped).await.is_err());
    source.close().await.unwrap();
    assert!(source.descriptor().await.is_err());
}

#[tokio::test]
async fn group_refuses_duplicate_source_owners_and_different_identity() {
    let root = tempdir();
    std::fs::write(
        root.path().join("session.jsonl"),
        journal("session-id", None),
    )
    .unwrap();
    let duplicate: Arc<dyn SnapshotExport> = Arc::new(source(
        root.path(),
        Domain::Sessions,
        vec![input("session.jsonl")],
    ));
    assert!(
        GroupSource::new(vec![duplicate.clone(), duplicate.clone()])
            .await
            .is_err()
    );
    assert!(duplicate.descriptor().await.is_err()); // Failed construction drains owners.
    let first: Arc<dyn SnapshotExport> = Arc::new(source(root.path(), Domain::Sessions, vec![]));
    let other: Arc<dyn SnapshotExport> = Arc::new(
        JournalSource::open(
            root.path(),
            Domain::Sessions,
            vec![],
            "different-source".into(),
            "freeze-1".into(),
            Arc::new(Fence),
        )
        .unwrap(),
    );
    assert!(GroupSource::new(vec![first, other]).await.is_err());
}
