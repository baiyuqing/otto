use super::*;
use std::path::PathBuf;

fn entries(list: &[&str]) -> Vec<Vec<u8>> {
    list.iter().map(|entry| entry.as_bytes().to_vec()).collect()
}

fn options(host: &[&str]) -> EnvironmentOptions {
    EnvironmentOptions {
        host_entries: entries(host),
        ..EnvironmentOptions::default()
    }
}

/// The resolved snapshot as a name/value map, checking on the way that no
/// entry is malformed or duplicated.
#[track_caller]
fn snapshot_environment(snapshot: &EnvironmentSnapshot) -> BTreeMap<&str, &str> {
    let mut environment = BTreeMap::new();
    for entry in snapshot.entries().expect("a resolved snapshot has entries") {
        let (name, value) = entry.split_once('=').expect("entry has a separator");
        assert!(!name.is_empty(), "entry has an empty name");
        assert!(
            environment.insert(name, value).is_none(),
            "duplicate name {name:?}"
        );
    }
    environment
}

#[track_caller]
fn assert_environment_names(snapshot: &EnvironmentSnapshot, want: &[&str]) {
    let got: Vec<&str> = snapshot_environment(snapshot).into_keys().collect();
    assert_eq!(got, want);
}

#[track_caller]
fn assert_redactions_contain(got: &[String], want: &[&str]) {
    for value in want {
        assert!(
            got.iter().any(|candidate| candidate == value),
            "redactions omitted {value:?}"
        );
    }
}

#[track_caller]
fn assert_redactions_sorted(values: &[String]) {
    assert!(
        values.windows(2).all(|pair| {
            pair[0].len() > pair[1].len() || (pair[0].len() == pair[1].len() && pair[0] <= pair[1])
        }),
        "redactions are not sorted longest-first then lexically: {values:?}"
    );
}

/// Three owner-only directories under one temporary root, returned with the
/// guard that keeps them alive.
fn new_private_directories() -> (tempfile::TempDir, PrivateDirectories) {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path().join("private");
    let directories = PrivateDirectories {
        temp: root.join("tmp"),
        cache: root.join("cache"),
        root,
    };
    for path in [&directories.root, &directories.temp, &directories.cache] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .expect("create private directory");
    }
    (temp, directories)
}

#[track_caller]
fn assert_unsafe_resolution(options: &EnvironmentOptions) {
    let rejection = resolve_environment(options).expect_err("resolution should be rejected");
    assert_eq!(rejection.to_string(), "sandbox environment is unsafe");
    assert_eq!(Error::from(rejection), Error::EnvironmentUnsafe);
}

fn sensitive_entries(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|index| {
            format!("VALUE_{index:03}_TOKEN=unique-sensitive-value-{index:03}").into_bytes()
        })
        .collect()
}

mod credential_names;
mod private_paths_and_bounds;
mod proxy_userinfo;
