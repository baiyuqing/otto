//! Tests for private path rewriting and redaction bounds.

use super::*;

#[test]
fn private_paths_are_rewritten_and_caches_created() {
    let (_guard, directories) = new_private_directories();
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "HOME=/host/home",
            "TMPDIR=/host/tmpdir",
            "TMP=/host/tmp",
            "TEMP=/host/temp",
            "TMPPREFIX=/host/zsh",
            "XDG_CACHE_HOME=/host/xdg",
            "GOCACHE=/host/go-build",
            "GOMODCACHE=/host/go-mod",
            "NPM_CONFIG_CACHE=/host/npm",
            "PIP_CACHE_DIR=/host/pip",
            "UV_CACHE_DIR=/host/uv",
            "CARGO_HOME=/host/cargo",
            "ZDOTDIR=/host/zdotdir",
            "home=preserved-lowercase",
            "HOME_SUFFIX=preserved-suffix",
        ]),
        private_directories: Some(directories.clone()),
        ..EnvironmentOptions::default()
    })
    .unwrap();

    let cache = |name: &str| display_path(&directories.cache.join(name));
    let want = BTreeMap::from([
        // The host `HOME` reaches the child; no private home directory exists.
        ("HOME", "/host/home".to_owned()),
        ("ZDOTDIR", display_path(&directories.temp)),
        ("CARGO_HOME", cache("cargo")),
        ("TMPDIR", display_path(&directories.temp)),
        ("TMP", display_path(&directories.temp)),
        ("TEMP", display_path(&directories.temp)),
        ("TMPPREFIX", display_path(&directories.temp.join("zsh"))),
        ("XDG_CACHE_HOME", cache("xdg")),
        ("GOCACHE", cache("go-build")),
        ("GOMODCACHE", cache("go-mod")),
        ("NPM_CONFIG_CACHE", cache("npm")),
        ("PIP_CACHE_DIR", cache("pip")),
        ("UV_CACHE_DIR", cache("uv")),
        ("home", "preserved-lowercase".to_owned()),
        ("HOME_SUFFIX", "preserved-suffix".to_owned()),
    ]);
    let got: BTreeMap<&str, String> = snapshot_environment(&snapshot)
        .into_iter()
        .map(|(name, value)| (name, value.to_owned()))
        .collect();
    assert_eq!(got, want);
    assert!(
        !got.contains_key("RUSTUP_HOME"),
        "RUSTUP_HOME must stay unset"
    );
    assert!(
        !directories.root.join("home").exists(),
        "no private home directory is created"
    );

    for name in DERIVED_CACHE_NAMES {
        let info = fs::symlink_metadata(directories.cache.join(name)).unwrap();
        assert!(info.is_dir() && !info.file_type().is_symlink(), "{name}");
        assert_eq!(info.mode() & 0o7777, 0o700, "{name}");
    }
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            &display_path(&directories.root),
            &display_path(&directories.temp),
            &display_path(&directories.cache),
        ],
    );
    assert_redactions_sorted(snapshot.redaction_values());
}

#[test]
fn direct_mode_preserves_host_paths() {
    let host = [
        "HOME=/host/home",
        "TMPDIR=/host/tmpdir",
        "TMP=/host/tmp",
        "TEMP=/host/temp",
        "XDG_CACHE_HOME=/host/xdg",
        "GOCACHE=/host/go-build",
        "GOMODCACHE=/host/go-mod",
        "NPM_CONFIG_CACHE=/host/npm",
        "PIP_CACHE_DIR=/host/pip",
        "UV_CACHE_DIR=/host/uv",
    ];
    let snapshot = resolve_environment(&options(&host)).unwrap();
    let mut sorted = host.to_vec();
    sorted.sort_unstable();
    assert_eq!(snapshot.entries().unwrap(), sorted);
    assert!(snapshot.redaction_values().is_empty());
}

#[test]
fn unsafe_private_directories_are_rejected() {
    let (_guard, directories) = new_private_directories();
    fs::remove_dir(&directories.cache).unwrap();
    std::os::unix::fs::symlink(tempfile::tempdir().unwrap().path(), &directories.cache).unwrap();
    assert_unsafe_resolution(&EnvironmentOptions {
        private_directories: Some(directories),
        ..EnvironmentOptions::default()
    });

    let (_guard, directories) = new_private_directories();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), directories.cache.join("xdg")).unwrap();
    assert_unsafe_resolution(&EnvironmentOptions {
        private_directories: Some(directories),
        ..EnvironmentOptions::default()
    });

    let (_guard, directories) = new_private_directories();
    fs::set_permissions(&directories.cache, fs::Permissions::from_mode(0o750)).unwrap();
    assert_unsafe_resolution(&EnvironmentOptions {
        private_directories: Some(directories),
        ..EnvironmentOptions::default()
    });

    let (_guard, directories) = new_private_directories();
    fs::DirBuilder::new()
        .mode(0o755)
        .create(directories.cache.join("xdg"))
        .unwrap();
    assert_unsafe_resolution(&EnvironmentOptions {
        private_directories: Some(directories),
        ..EnvironmentOptions::default()
    });
}

#[test]
fn json_string_escape_forms_expand_before_the_bounds() {
    let raw = r"prefix\u003csuffix";
    let snapshot = resolve_environment(&options(&[&format!("ESCAPED_TOKEN={raw}")])).unwrap();
    assert_redactions_contain(snapshot.redaction_values(), &[raw, "prefix<suffix"]);

    let mut host = sensitive_entries(511);
    host.push(format!("ESCAPED_TOKEN={raw}").into_bytes());
    let rejection = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        ..EnvironmentOptions::default()
    })
    .unwrap_err();
    assert!(!rejection.snapshot().redactions_complete());
}

#[test]
fn the_sensitive_value_count_bound_is_enforced() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: sensitive_entries(512),
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_eq!(snapshot.redaction_values().len(), 512);

    assert_unsafe_resolution(&EnvironmentOptions {
        host_entries: sensitive_entries(513),
        ..EnvironmentOptions::default()
    });
}

#[test]
fn incomplete_redaction_snapshots_are_marked() {
    let complete = resolve_environment(&EnvironmentOptions {
        host_entries: sensitive_entries(512),
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert!(complete.redactions_complete());

    let overflow = resolve_environment(&EnvironmentOptions {
        host_entries: sensitive_entries(513),
        ..EnvironmentOptions::default()
    })
    .unwrap_err();
    assert!(!overflow.snapshot().redactions_complete());
    assert!(
        !overflow
            .snapshot()
            .redaction_values()
            .iter()
            .any(|value| value == "unique-sensitive-value-512")
    );

    let over_budget = resolve_environment(&options(&[&format!(
        "LARGE_TOKEN={}",
        "z".repeat((1 << 20) + 1)
    )]))
    .unwrap_err();
    assert!(!over_budget.snapshot().redactions_complete());

    let malformed = resolve_environment(&options(&["BROKEN"])).unwrap_err();
    assert!(!malformed.snapshot().redactions_complete());

    let malformed_proxy =
        resolve_environment(&options(&["HTTPS_PROXY=http:///user:pass@example.test"])).unwrap_err();
    assert!(!malformed_proxy.snapshot().redactions_complete());
}

#[test]
fn the_sensitive_byte_bound_is_enforced_before_private_paths() {
    let at_limit = "x".repeat(1 << 20);
    let snapshot = resolve_environment(&options(&[&format!("LARGE_TOKEN={at_limit}")])).unwrap();
    assert_eq!(snapshot.redaction_values().len(), 1);
    assert_eq!(snapshot.redaction_values()[0].len(), 1 << 20);

    let (_guard, directories) = new_private_directories();
    assert_unsafe_resolution(&EnvironmentOptions {
        host_entries: entries(&[&format!("LARGE_TOKEN={}", "y".repeat((1 << 20) + 1))]),
        private_directories: Some(directories.clone()),
        ..EnvironmentOptions::default()
    });
    for name in DERIVED_CACHE_NAMES {
        assert!(
            fs::symlink_metadata(directories.cache.join(name)).is_err(),
            "derived cache {name} was created before the bound passed"
        );
    }
}

#[test]
fn private_path_redactions_stay_inside_the_sensitive_byte_bound() {
    let directories = PrivateDirectories {
        root: PathBuf::from("r".repeat(1 << 20)),
        temp: PathBuf::from("/known/private/temp"),
        cache: PathBuf::from("/known/private/cache"),
    };
    let rejection = resolve_environment(&EnvironmentOptions {
        private_directories: Some(directories),
        ..EnvironmentOptions::default()
    })
    .unwrap_err();
    assert!(rejection.snapshot().entries().is_none());
    let total: usize = rejection
        .snapshot()
        .redaction_values()
        .iter()
        .map(String::len)
        .sum();
    assert!(total <= 1 << 20, "private redaction bytes = {total}");
}

#[test]
fn malformed_repeated_proxy_prefixes_stay_bounded() {
    let mut proxy = String::from("http:///");
    for index in 0..600 {
        proxy.push_str(&format!("u{index:03}:p{index:03}@"));
    }
    proxy.push_str("example.test");
    let rejection = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap_err();
    assert!(!rejection.snapshot().redactions_complete());
    assert!(rejection.snapshot().redaction_values().len() <= 512);
    assert!(
        !rejection
            .snapshot()
            .redaction_values()
            .iter()
            .any(|value| value == "u000:p000@u001:p001")
    );
}

#[test]
fn resolution_returns_sorted_entries_and_redactions() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "ZED=ordinary",
            "ALPHA_TOKEN=longest-sensitive-value",
            "BETA_SECRET=aa",
            "GAMMA_PASSWORD=zz",
        ]),
        allow_names: vec!["ALPHA_TOKEN".into()],
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_eq!(
        snapshot.entries().unwrap(),
        ["ALPHA_TOKEN=longest-sensitive-value", "ZED=ordinary"]
    );
    assert_eq!(
        snapshot.redaction_values(),
        ["longest-sensitive-value", "aa", "zz"]
    );

    let empty = resolve_environment(&options(&[])).unwrap();
    assert_eq!(empty.entries(), Some(&[][..]));
    assert!(empty.redaction_values().is_empty());
}

#[test]
fn invalid_option_names_are_rejected() {
    assert_unsafe_resolution(&EnvironmentOptions {
        provider_names: vec!["INVALID-NAME".into()],
        ..EnvironmentOptions::default()
    });
    assert_unsafe_resolution(&EnvironmentOptions {
        allow_names: vec!["*_TOKEN".into()],
        ..EnvironmentOptions::default()
    });
    assert_unsafe_resolution(&EnvironmentOptions {
        allow_names: vec!["DUPLICATE".into(), "DUPLICATE".into()],
        ..EnvironmentOptions::default()
    });
}
