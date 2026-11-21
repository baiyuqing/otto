use super::super::lifecycle::list_request;
use super::*;

fn open_without_wait() -> (tempfile::TempDir, FileStore) {
    let (dir, store) = open_temp();
    let options = MemoryOptions {
        busy_timeout: Duration::ZERO,
        ..MemoryOptions::default()
    };
    let store = FileStore::open(store.root(), options).unwrap();
    (dir, store)
}

#[test]
fn reads_take_no_lock_while_all_evidence_is_current() {
    let (_dir, store) = open_without_wait();
    let scope = user(&store);
    let source = put(&store, "a", &scope, "", "source");
    let dependent = dependent(&store, &scope, "b", &source);
    let _held = acquire(store.root(), Duration::ZERO).unwrap();
    assert_eq!(get(&store, &dependent).state, RecordState::Active);
    assert_eq!(store.list(&list_request(&scope)).unwrap().records.len(), 2);
}

#[test]
fn a_read_that_must_reconcile_takes_the_lock() {
    let (_dir, store) = open_without_wait();
    let scope = user(&store);
    let source = put(&store, "a", &scope, "", "source");
    let dependent = dependent(&store, &scope, "b", &source);
    let mut newer = source.clone();
    newer.revision += 1;
    store.put_record(&newer).unwrap();
    let reference = record_ref(&scope, &dependent.id);
    let held = acquire(store.root(), Duration::ZERO).unwrap();
    assert_eq!(store.get(&reference).unwrap_err().kind, ErrorKind::Busy);
    drop(held);
    assert_eq!(
        store.get(&reference).unwrap().state,
        RecordState::NeedsReview
    );
}
