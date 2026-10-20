
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

/// Four owner-only directories under one temporary root, returned with the
/// guard that keeps them alive.
fn new_private_directories() -> (tempfile::TempDir, PrivateDirectories) {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path().join("private");
    let directories = PrivateDirectories {
        home: root.join("home"),
        temp: root.join("tmp"),
        cache: root.join("cache"),
        root,
    };
    for path in [
        &directories.root,
        &directories.home,
        &directories.temp,
        &directories.cache,
    ] {
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

#[test]
fn acp_token_is_redacted_and_cannot_be_restored_to_tools() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: vec![b"OTTO_ACP_TOKEN=test-listener-secret".to_vec()],
        allow_names: vec!["OTTO_ACP_TOKEN".into()],
        ..Default::default()
    })
    .expect("environment");
    assert!(
        !snapshot
            .entries()
            .unwrap()
            .iter()
            .any(|entry| entry.starts_with("OTTO_ACP_TOKEN="))
    );
    assert!(
        snapshot
            .redaction_values()
            .iter()
            .any(|value| value == "test-listener-secret")
    );
}

#[test]
fn a_failed_resolution_retains_bounded_redactions_from_every_valid_entry() {
    let (_guard, directories) = new_private_directories();
    let rejection = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "AWS_SECRET_ACCESS_KEY=first-aws-secret",
            "HTTPS_PROXY=http://raw%20user:raw%2Fpass@[::1]:8443/path",
            "BROKEN",
            "AWS_SECRET_ACCESS_KEY=second-aws-secret",
        ]),
        private_directories: Some(directories.clone()),
        ..EnvironmentOptions::default()
    })
    .expect_err("a malformed entry rejects the environment");
    let snapshot = rejection.snapshot();
    assert!(snapshot.entries().is_none());
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "first-aws-secret",
            "second-aws-secret",
            "raw%20user:raw%2Fpass",
            "raw%20user",
            "raw%2Fpass",
            "raw user:raw/pass",
            "raw user",
            "raw/pass",
            &display_path(&directories.root),
            &display_path(&directories.home),
            &display_path(&directories.temp),
            &display_path(&directories.cache),
        ],
    );
    assert_redactions_sorted(snapshot.redaction_values());
}

#[test]
fn provider_credentials_are_removed() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "OTTO_API_KEY=otto-sensitive-value",
            "otto_api_key=lower-otto-sensitive-value",
            "selected_key=selected-sensitive-value",
            "CONFIGURED_KEY=configured-sensitive-value",
            "ORDINARY=preserved",
        ]),
        provider_names: vec!["SELECTED_KEY".into(), "CONFIGURED_KEY".into()],
        allow_names: vec![
            "OTTO_API_KEY".into(),
            "otto_api_key".into(),
            "selected_key".into(),
            "CONFIGURED_KEY".into(),
        ],
        private_directories: None,
    })
    .unwrap();
    assert_environment_names(&snapshot, &["ORDINARY"]);
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "otto-sensitive-value",
            "lower-otto-sensitive-value",
            "selected-sensitive-value",
            "configured-sensitive-value",
        ],
    );
}

#[test]
fn loader_and_shell_injection_variables_are_never_restored() {
    let names = [
        "DYLD_INSERT_LIBRARIES",
        "dyld_custom",
        "LD_PRELOAD",
        "ld_library_path",
        "BASH_ENV",
        "env",
        "ZDOTDIR",
        "PROMPT_COMMAND",
        "CDPATH",
        "SHELLOPTS",
        "BASHOPTS",
    ];
    let mut host: Vec<Vec<u8>> = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=classified-value-{index:02}").into_bytes())
        .collect();
    host.push(b"AUTHORS=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        allow_names: names.iter().map(|name| (*name).to_owned()).collect(),
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["AUTHORS"]);
    assert_eq!(snapshot.redaction_values().len(), names.len());
}

#[test]
fn internal_and_control_variables_are_never_restored() {
    let names = [
        "OTTO_SANDBOX",
        "otto_sandbox_profile_path",
        "SSH_AUTH_SOCK",
        "docker_host",
        "CONTAINER_HOST",
    ];
    let mut host: Vec<Vec<u8>> = Vec::new();
    let mut original_values: Vec<String> = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let value = format!("non-restorable-value-{index:02}");
        host.push(format!("{name}={value}").into_bytes());
        original_values.push(value);
    }
    host.push(b"SANDBOX_PROFILE=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        allow_names: names.iter().map(|name| (*name).to_owned()).collect(),
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["SANDBOX_PROFILE"]);
    let wanted: Vec<&str> = original_values.iter().map(String::as_str).collect();
    assert_redactions_contain(snapshot.redaction_values(), &wanted);
}

#[test]
fn sensitive_suffixes_are_classified_case_insensitively() {
    let names = [
        "BUILD_TOKEN",
        "build_secret",
        "DATABASE_PASSWORD",
        "database_passwd",
        "SERVICE_API_KEY",
        "service_access_key",
        "signing_private_key",
        "APP_CREDENTIAL",
        "app_credentials",
    ];
    let mut host: Vec<Vec<u8>> = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=suffix-sensitive-{index:02}").into_bytes())
        .collect();
    host.push(b"AUTHORS=preserved".to_vec());
    host.push(b"PASSWORD_POLICY=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["AUTHORS", "PASSWORD_POLICY"]);
    assert_eq!(snapshot.redaction_values().len(), names.len());
}

#[test]
fn fixed_credential_names_are_classified() {
    let names = [
        "AWS_ACCESS_KEY_ID",
        "azure_client_id",
        "GCP_SERVICE_ACCOUNT_KEY",
        "github_pat",
        "GITLAB_CI_JOB_JWT",
        "NPM_CONFIG__AUTH",
        "REGISTRY_AUTH_FILE",
        "DOCKER_AUTH_CONFIG",
        "CI_JOB_JWT",
        "AUTHORIZATION",
        "http_cookie",
        "SSH_AUTH_SOCK",
        "DOCKER_HOST",
        "CONTAINER_HOST",
    ];
    let mut host: Vec<Vec<u8>> = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=fixed-sensitive-{index:02}").into_bytes())
        .collect();
    host.push(b"AUTHORS=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["AUTHORS"]);
    assert_eq!(snapshot.redaction_values().len(), names.len());
}

#[test]
fn an_exact_allow_restores_only_automatic_non_provider_names() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "PROJECT_TOKEN=restored-sensitive-value",
            "project_token=case-sensitive-not-restored",
            "NPM_CONFIG__AUTH=restored-fixed-value",
            "PROVIDER_TOKEN=provider-sensitive-value",
            "BASH_ENV=shell-sensitive-value",
            "ORDINARY=preserved",
        ]),
        provider_names: vec!["PROVIDER_TOKEN".into()],
        allow_names: vec![
            "PROJECT_TOKEN".into(),
            "NPM_CONFIG__AUTH".into(),
            "PROVIDER_TOKEN".into(),
            "BASH_ENV".into(),
        ],
        private_directories: None,
    })
    .unwrap();
    assert_environment_names(
        &snapshot,
        &["NPM_CONFIG__AUTH", "ORDINARY", "PROJECT_TOKEN"],
    );
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "restored-sensitive-value",
            "case-sensitive-not-restored",
            "restored-fixed-value",
            "provider-sensitive-value",
            "shell-sensitive-value",
        ],
    );
}

#[test]
fn proxy_userinfo_is_collected_without_rewriting_the_proxy() {
    let proxy = "http://raw%20user:raw%2Fpass@[2001:db8::1]:8443/path";
    let second = "socks5://solo%20user@[::1]:9000";
    let network_path = "//network%20user:network%2Fpass@[2001:db8::2]:9443/path@ignored";
    let snapshot = resolve_environment(&options(&[
        &format!("HTTPS_PROXY={proxy}"),
        &format!("all_proxy={second}"),
        &format!("http_proxy={network_path}"),
    ]))
    .unwrap();
    let environment = snapshot_environment(&snapshot);
    assert_eq!(environment["HTTPS_PROXY"], proxy);
    assert_eq!(environment["all_proxy"], second);
    assert_eq!(environment["http_proxy"], network_path);
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "raw%20user:raw%2Fpass",
            "raw user:raw/pass",
            "raw user",
            "raw/pass",
            "solo%20user",
            "solo user",
            "network%20user:network%2Fpass",
            "network user:network/pass",
        ],
    );
    assert_redactions_sorted(snapshot.redaction_values());
}

#[test]
fn malformed_proxy_authorities_are_extracted_conservatively() {
    let cases: &[(&str, &[&str])] = &[
        (
            "http:///proxy-user:proxy-pass@example.test",
            &["proxy-user:proxy-pass", "proxy-user", "proxy-pass"],
        ),
        (
            r"http:\\proxy%20user:proxy%2Fpass@example.test\path",
            &[
                "proxy%20user:proxy%2Fpass",
                "proxy%20user",
                "proxy%2Fpass",
                "proxy user:proxy/pass",
                "proxy user",
                "proxy/pass",
            ],
        ),
        (
            "proxy%20user:proxy%2Fpass@example.test/path",
            &[
                "proxy%20user:proxy%2Fpass",
                "proxy%20user",
                "proxy%2Fpass",
                "proxy user:proxy/pass",
                "proxy user",
                "proxy/pass",
            ],
        ),
        (
            "ht!tp://odd%20user:odd%2Fpass@example.test/path",
            &[
                "odd%20user:odd%2Fpass",
                "odd%20user",
                "odd%2Fpass",
                "odd user:odd/pass",
                "odd user",
                "odd/pass",
            ],
        ),
        (
            "http:odd%20user:odd%2Fpass@example.test/path",
            &[
                "http:odd%20user:odd%2Fpass",
                "odd%20user:odd%2Fpass",
                "odd%20user",
                "odd%2Fpass",
                "odd user",
                "odd/pass",
            ],
        ),
        (
            "http://[::1/path/proxy%20user:proxy%2Fpass@example.test",
            &[
                "proxy%20user:proxy%2Fpass",
                "proxy%20user",
                "proxy%2Fpass",
                "proxy user",
                "proxy/pass",
            ],
        ),
        (
            "http://bad%zz:pass%2Fword@[::1]:8080/path?next=@ignored#@ignored",
            &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
        ),
        (
            "http://user%20name:bad%zz@[2001:db8::1]:8080/path",
            &["user%20name:bad%zz", "user%20name", "bad%zz", "user name"],
        ),
        (
            "http://bad%zz:pass%2Fword@[::1]:8080/path/user:other@example.test",
            &["bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"],
        ),
        (
            "http:///real%20user:real%2Fpass@proxy/path%20user:path%2Fpass@example",
            &[
                "real%20user:real%2Fpass",
                "real user:real/pass",
                "path%20user:path%2Fpass",
                "path user:path/pass",
            ],
        ),
        (
            "///raw%20user:raw%2Fpass@host/path@ignored",
            &["raw%20user:raw%2Fpass", "raw user:raw/pass", "path"],
        ),
    ];
    for (proxy, want) in cases {
        let Err(rejection) = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")]))
        else {
            panic!("{proxy} should be rejected");
        };
        let snapshot = rejection.snapshot();
        assert!(snapshot.entries().is_none(), "{proxy}");
        assert!(!snapshot.redactions_complete(), "{proxy}");
        assert_redactions_contain(snapshot.redaction_values(), want);
    }
}

#[test]
fn percent_decoded_invalid_utf8_proxy_userinfo_is_canonicalized() {
    let proxy = "http://user%FF:pass%C0%AF@[::1]:8080/path";
    let snapshot = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap();
    assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], proxy);
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "user%FF:pass%C0%AF",
            "user%FF",
            "pass%C0%AF",
            "user\u{fffd}:pass\u{fffd}\u{fffd}",
            "user\u{fffd}",
            "pass\u{fffd}\u{fffd}",
        ],
    );
}

#[test]
fn a_valid_proxy_path_query_or_fragment_at_is_not_userinfo() {
    let proxy = "http://[2001:db8::1]:8080/path/user:pass@example.test?next=user:pass@example.test#user:pass@example.test";
    let snapshot = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap();
    assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], proxy);
    assert!(snapshot.redaction_values().is_empty());
}

#[test]
fn a_malformed_proxy_path_at_is_incomplete_without_inventing_userinfo() {
    let proxy = "http://[2001:db8::1]:8080/path/user:pass@example.test?broken=%zz";
    let rejection = resolve_environment(&options(&[&format!("HTTPS_PROXY={proxy}")])).unwrap_err();
    assert!(rejection.snapshot().entries().is_none());
    assert!(!rejection.snapshot().redactions_complete());
    assert!(rejection.snapshot().redaction_values().is_empty());
}

#[test]
fn proxy_values_without_a_literal_at_are_preserved() {
    for value in [
        "http://example.test/path%zz",
        "http:///example.test/path",
        r"http:\\example.test\path",
        "example.test:8080",
        "//example.test:8443",
        "http://:8080",
        "http://example.test:99999",
    ] {
        let Ok(snapshot) = resolve_environment(&options(&[&format!("HTTPS_PROXY={value}")])) else {
            panic!("{value} was rejected");
        };
        assert!(snapshot.redactions_complete(), "{value}");
        assert_eq!(snapshot_environment(&snapshot)["HTTPS_PROXY"], value);
    }
}

#[test]
fn no_proxy_bypass_lists_are_preserved_case_insensitively() {
    let host = [
        "NO_PROXY=localhost,127.0.0.1,.example.test,[::1],*",
        "no_proxy=user:pass@example.test,service.local",
    ];
    let snapshot = resolve_environment(&options(&host)).unwrap();
    assert!(snapshot.redactions_complete());
    assert_eq!(snapshot.entries().unwrap(), host);
    assert!(snapshot.redaction_values().is_empty());
}

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
            "home=preserved-lowercase",
            "HOME_SUFFIX=preserved-suffix",
        ]),
        private_directories: Some(directories.clone()),
        ..EnvironmentOptions::default()
    })
    .unwrap();

    let cache = |name: &str| display_path(&directories.cache.join(name));
    let want = BTreeMap::from([
        ("HOME", display_path(&directories.home)),
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

    for name in DERIVED_CACHE_NAMES {
        let info = fs::symlink_metadata(directories.cache.join(name)).unwrap();
        assert!(info.is_dir() && !info.file_type().is_symlink(), "{name}");
        assert_eq!(info.mode() & 0o7777, 0o700, "{name}");
    }
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            &display_path(&directories.root),
            &display_path(&directories.home),
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
        home: PathBuf::from("/known/private/home"),
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
