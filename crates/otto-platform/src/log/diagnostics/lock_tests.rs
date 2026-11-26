use super::*;

#[test]
fn new_owner_lock_waits_for_a_brief_cleanup_probe() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("probe.lock");
    let owner = File::create(&path).unwrap();
    let probe = File::open(&path).unwrap();
    fsops::try_lock_exclusive(&probe).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(probe);
    });
    lock_new_owner(&owner).expect("lock acquired after the probe is released");
    release.join().unwrap();
    let competitor = File::open(&path).unwrap();
    assert_eq!(
        fsops::try_lock_exclusive(&competitor).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(owner);
    fsops::try_lock_exclusive(&competitor).expect("owner release frees the lock");
}
