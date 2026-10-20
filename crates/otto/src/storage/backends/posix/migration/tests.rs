
use super::*;
use crate::storage::migration::{DomainDigest, DomainSnapshot, FrozenSnapshot, Progress};

fn tempdir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

struct Stopped;
impl StoppedOwners for Stopped {
    fn verify_stopped(&self) -> Result<()> {
        Ok(())
    }
}
struct Busy;
impl StoppedOwners for Busy {
    fn verify_stopped(&self) -> Result<()> {
        Err(conflict("actual owner lock busy"))
    }
}

fn routes(endpoint: &str) -> BTreeMap<String, StorageRoute> {
    BTreeMap::from([(
        "history".into(),
        StorageRoute {
            backend: "posix".into(),
            endpoint: endpoint.into(),
            namespace_identity: None,
        },
    )])
}
fn initial() -> RouteGroup {
    RouteGroup {
        identity: "source".into(),
        generation: "route-1".into(),
        writer_epoch: "epoch-1".into(),
        domains: routes("/source"),
    }
}
fn prepared() -> PreparedCutover {
    PreparedCutover {
        migration_id: "move/1".into(),
        source_identity: "source".into(),
        destination_identity: "destination".into(),
        source_generation: "frozen-1".into(),
        expected_route_generation: "route-1".into(),
        source_routes: routes("/source"),
        destination_routes: routes("/destination"),
    }
}
fn manifest() -> Manifest {
    Manifest {
        format_version: FORMAT_VERSION,
        max_chunk_bytes: 1024,
        migration_id: "move/1".into(),
        destination_identity: "destination".into(),
        expected_route_generation: "route-1".into(),
        source: FrozenSnapshot {
            source_identity: "source".into(),
            generation: "frozen-1".into(),
            domains: BTreeMap::from([(
                "history".into(),
                DomainSnapshot {
                    schema_family: "history".into(),
                    schema_version: 1,
                    committed_cursor: "1".into(),
                    digest: DomainDigest {
                        records: 0,
                        blob_bytes: 0,
                        sha256: "digest".into(),
                    },
                },
            )]),
        },
        progress: BTreeMap::from([(
            "history".into(),
            Progress {
                cursor: String::new(),
                complete: false,
                chunks: vec![],
            },
        )]),
        destination_write_started: false,
        sealed: false,
        published_generation: None,
        rolled_back: false,
    }
}

#[tokio::test]
async fn unplanned_abort_and_late_checkpoint_creation_are_mutually_exclusive() {
    let root = tempdir();
    let control = LocalCutover::open(root.path(), initial()).unwrap();
    let journal = LocalJournal::open(root.path()).unwrap();
    control.freeze(prepared(), Arc::new(Stopped)).await.unwrap();
    let manifest = manifest();
    let (abort, save) = tokio::join!(
        control.abort_unplanned("move/1"),
        journal.save(None, &manifest)
    );
    assert_ne!(abort.is_ok(), save.is_ok());
    if abort.is_ok() {
        control.ensure_writable("source", "route-1").await.unwrap();
        assert!(journal.save(None, &manifest).await.is_err());
    } else {
        assert!(journal.load("move/1").await.unwrap().is_some());
        assert!(control.abort_unplanned("move/1").await.is_err());
        assert!(control.ensure_writable("source", "route-1").await.is_err());
    }
}

#[tokio::test]
async fn first_freeze_adopts_verified_implicit_source_routes_and_rollback_retains_them() {
    let root = tempdir();
    let mut implicit = initial();
    implicit.domains.clear();
    let control = LocalCutover::open(root.path(), implicit).unwrap();
    assert!(control.routes().await.unwrap().domains.is_empty());
    assert!(control.freeze(prepared(), Arc::new(Busy)).await.is_err());
    assert!(control.routes().await.unwrap().domains.is_empty());
    control.freeze(prepared(), Arc::new(Stopped)).await.unwrap();
    assert_eq!(control.routes().await.unwrap().domains, routes("/source"));
    let mut manifest = manifest();
    manifest.rolled_back = true;
    LocalJournal::open(root.path())
        .unwrap()
        .save(None, &manifest)
        .await
        .unwrap();
    control.rollback(&manifest).await.unwrap();
    assert_eq!(control.routes().await.unwrap().domains, routes("/source"));
    control.ensure_writable("source", "route-1").await.unwrap();
}

#[tokio::test]
async fn multiple_supported_writers_share_admission_and_block_freeze() {
    let root = tempdir();
    let control = LocalCutover::open(root.path(), initial()).unwrap();
    let first = control.acquire_writer("source", "route-1").await.unwrap();
    let second = control.acquire_writer("source", "route-1").await.unwrap();
    assert!(matches!(
        control.freeze(prepared(), Arc::new(Stopped)).await,
        Err(Error::Conflict(_))
    ));
    drop(first);
    assert!(control.freeze(prepared(), Arc::new(Stopped)).await.is_err());
    drop(second);
    control.freeze(prepared(), Arc::new(Stopped)).await.unwrap();
    assert!(control.acquire_writer("source", "route-1").await.is_err());
}

#[tokio::test]
async fn distinct_cas_updates_on_one_adapter_do_not_both_commit() {
    let root = tempdir();
    let journal = LocalJournal::open(root.path()).unwrap();
    let mut left = manifest();
    journal.save(None, &left).await.unwrap();
    let mut right = left.clone();
    left.destination_write_started = true;
    right.rolled_back = true;
    let (left, right) = tokio::join!(journal.save(Some(1), &left), journal.save(Some(1), &right));
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert!(matches!(left, Err(Error::Conflict(_))) || matches!(right, Err(Error::Conflict(_))));
}

#[tokio::test]
async fn durable_cas_reopen_and_duplicate_checkpoint() {
    let root = tempdir();
    let first = LocalJournal::open(root.path()).unwrap();
    let mut manifest = manifest();
    assert_eq!(first.save(None, &manifest).await.unwrap(), 1);
    let second = LocalJournal::open(root.path()).unwrap();
    assert_eq!(
        second.load("move/1").await.unwrap().unwrap().manifest,
        manifest
    );
    assert_eq!(second.save(None, &manifest).await.unwrap(), 1);
    manifest.destination_write_started = true;
    let (left, right) = tokio::join!(
        first.save(Some(1), &manifest),
        second.save(Some(1), &manifest)
    );
    assert_eq!(left.unwrap(), 2);
    assert_eq!(right.unwrap(), 2);
    manifest.sealed = true;
    assert!(matches!(
        first.save(Some(1), &manifest).await,
        Err(Error::Conflict(_))
    ));
    manifest.destination_write_started = false;
    assert!(matches!(
        first.save(Some(2), &manifest).await,
        Err(Error::Conflict(_))
    ));
    manifest.destination_write_started = true;
    manifest.source.generation = "different-source".into();
    assert!(matches!(
        first.save(Some(2), &manifest).await,
        Err(Error::Conflict(_))
    ));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(root.path().join(journal_name("move/1").unwrap()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn freeze_survives_reopen_and_publish_is_one_receipted_generation() {
    let root = tempdir();
    let control = LocalCutover::open(root.path(), initial()).unwrap();
    assert!(matches!(
        control.freeze(prepared(), Arc::new(Busy)).await,
        Err(Error::Conflict(_))
    ));
    control.freeze(prepared(), Arc::new(Stopped)).await.unwrap();
    drop(control);
    let control = LocalCutover::open(root.path(), initial()).unwrap();
    control.validate_freeze("frozen-1").await.unwrap();
    assert!(control.ensure_writable("source", "route-1").await.is_err());
    let journal = LocalJournal::open(root.path()).unwrap();
    let mut manifest = manifest();
    manifest.destination_write_started = true;
    manifest.sealed = true;
    manifest.progress.get_mut("history").unwrap().complete = true;
    journal.save(None, &manifest).await.unwrap();
    let generation = control.publish(&manifest).await.unwrap();
    let reopened = LocalCutover::open(root.path(), initial()).unwrap();
    assert_eq!(
        reopened.receipt("move/1").await.unwrap(),
        Some(generation.clone())
    );
    assert_eq!(reopened.publish(&manifest).await.unwrap(), generation);
    let mut changed = manifest.clone();
    changed
        .source
        .domains
        .get_mut("history")
        .unwrap()
        .digest
        .records = 9;
    assert!(matches!(
        reopened.publish(&changed).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        reopened.routes().await.unwrap().domains,
        routes("/destination")
    );
    reopened.validate_freeze("frozen-1").await.unwrap();
    reopened
        .ensure_writable("destination", &generation)
        .await
        .unwrap();
    assert!(reopened.ensure_writable("source", "route-1").await.is_err());
    assert!(reopened.rollback(&manifest).await.is_err());
}

#[tokio::test]
async fn rollback_releases_unpublished_source_with_isolated_stage_writes_and_is_durable() {
    let root = tempdir();
    let control = LocalCutover::open(root.path(), initial()).unwrap();
    control.freeze(prepared(), Arc::new(Stopped)).await.unwrap();
    let mut manifest = manifest();
    manifest.destination_write_started = true;
    manifest.rolled_back = true;
    LocalJournal::open(root.path())
        .unwrap()
        .save(None, &manifest)
        .await
        .unwrap();
    control.rollback(&manifest).await.unwrap();
    let reopened = LocalCutover::open(root.path(), initial()).unwrap();
    reopened.rollback(&manifest).await.unwrap();
    reopened.ensure_writable("source", "route-1").await.unwrap();
    assert!(reopened.validate_freeze("frozen-1").await.is_err());
}

#[tokio::test]
async fn corrupted_files_and_symlinked_roots_or_checkpoints_fail_closed() {
    use std::os::unix::fs::symlink;
    let root = tempdir();
    let outside = tempdir();
    symlink(outside.path(), root.path().join("linked")).unwrap();
    std::fs::create_dir(outside.path().join("nested")).unwrap();
    assert!(LocalJournal::open(&root.path().join("linked/nested")).is_err());
    let journal = LocalJournal::open(root.path()).unwrap();
    let checkpoint = root.path().join(journal_name("move/1").unwrap());
    std::fs::write(&checkpoint, b"broken").unwrap();
    assert!(journal.load("move/1").await.is_err());
    assert!(journal.save(None, &manifest()).await.is_err());
    std::fs::remove_file(&checkpoint).unwrap();
    std::fs::write(outside.path().join("untouched"), b"original").unwrap();
    symlink(outside.path().join("untouched"), &checkpoint).unwrap();
    assert!(journal.save(None, &manifest()).await.is_err());
    assert_eq!(
        std::fs::read(outside.path().join("untouched")).unwrap(),
        b"original"
    );
    std::fs::write(root.path().join(AUTHORITY), b"broken").unwrap();
    assert!(LocalCutover::open(root.path(), initial()).is_err());
}
