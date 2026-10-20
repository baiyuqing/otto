use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::faults::publish::FaultyPublishOps;
use crate::faults::{Fault, FaultError, FaultPlan, Rule, Trigger};

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write fixture");
    path
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = match fs::read_dir(dir.join("backups")) {
        Ok(entries) => entries.map(|entry| entry.expect("entry").path()).collect(),
        Err(_) => Vec::new(),
    };
    paths.sort();
    paths
}

fn profile_file(name: &str) -> Vec<u8> {
    format!("default_profile = \"{name}\"\n").into_bytes()
}

#[test]
fn create_initial_profile_refuses_an_existing_empty_file() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, b"").expect("empty config");

    let error = create_initial_profile(&path, "default", PROVIDER_CHATGPT, "my-model", "", "")
        .expect_err("must not replace an existing config");

    assert_eq!(error, "configuration already exists; no changes made");
    assert_eq!(fs::read(&path).expect("read"), b"");
    assert!(backups(directory.path()).is_empty());
}

#[test]
fn create_initial_profile_writes_only_non_secret_fields() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");

    create_initial_profile(
        &path,
        "default",
        PROVIDER_OPENAI_COMPATIBLE,
        "cheap-model",
        "https://api.example/v1",
        "EXAMPLE_KEY",
    )
    .expect("create");

    let content = fs::read_to_string(&path).expect("read");
    assert!(
        content.contains("api_key_env = \"EXAMPLE_KEY\""),
        "{content}"
    );
    assert!(!content.contains("configured"), "{content}");
    let parsed = otto_core::config::parse(&content).expect("parse");
    assert!(!content.contains("subagent_models"), "{content}");
    assert!(parsed.profiles["default"].subagent_models.is_empty());
    assert!(backups(directory.path()).is_empty());
}

#[test]
fn parent_directory_of_a_bare_file_name_is_the_working_directory() {
    assert_eq!(parent_directory(Path::new("config.toml")), Path::new("."));
    assert_eq!(
        parent_directory(Path::new("/home/u/.config/otto/config.toml")),
        Path::new("/home/u/.config/otto")
    );
}

#[test]
fn write_refuses_a_write_when_the_file_changed_since_it_was_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let concurrent = "default_profile = \"written by another otto\"\n";
    let path = write(dir.path(), "config.toml", concurrent);

    let error = write_bytes(&path, b"what this process read", &profile_file("mine"))
        .expect_err("stale write");

    assert!(error.to_string().contains("changed on disk"), "{error}");
    assert_eq!(fs::read_to_string(&path).expect("read"), concurrent);
    assert!(backups(dir.path()).is_empty());
}

fn write_failing_at(step: PublishStep) -> (tempfile::TempDir, PathBuf, Result<(), &'static str>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write(dir.path(), "config.toml", "default_profile = \"old\"\n");
    let plan = FaultPlan::new(vec![Rule::new(
        step,
        Trigger::Always,
        Fault::Before(FaultError::Os(libc::EIO)),
    )]);
    let result = write_bytes_with(
        &path,
        b"default_profile = \"old\"\n",
        &profile_file("new"),
        &FaultyPublishOps::new(plan.clone()),
    );
    plan.assert_all_fired();
    (dir, path, result)
}

fn temporary_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name().into_string().unwrap())
        .filter(|name| name.starts_with(".otto-config-"))
        .collect()
}

#[test]
fn a_failure_before_the_rename_keeps_the_config_and_leaves_no_temporary_file() {
    for (step, message) in [
        (PublishStep::FileSync, "cannot write configuration"),
        (PublishStep::Rename, "cannot replace configuration"),
    ] {
        let (dir, path, result) = write_failing_at(step);
        assert_eq!(result.expect_err("injected failure"), message, "{step:?}");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "default_profile = \"old\"\n",
            "{step:?}"
        );
        assert!(temporary_files(dir.path()).is_empty(), "{step:?}");
    }
}

#[test]
fn a_crash_before_the_config_rename_keeps_the_previous_config_at_the_target() {
    use crate::faults::crash::{self, CRASH_EXIT, Role};
    crash::run(crash::name_of(&|| {}), |role| match role {
        Role::Child(dir) => {
            let path = write(dir, "config.toml", "default_profile = \"old\"\n");
            let plan = FaultPlan::new(vec![Rule::new(
                PublishStep::Rename,
                Trigger::Always,
                Fault::Exit,
            )]);
            let _ = write_bytes_with(
                &path,
                b"default_profile = \"old\"\n",
                &profile_file("new"),
                &FaultyPublishOps::new(plan),
            );
        }
        Role::Parent(dir, status) => {
            assert_eq!(
                status.code(),
                Some(CRASH_EXIT),
                "child did not crash at the fault"
            );
            let path = dir.join("config.toml");
            assert_eq!(
                fs::read_to_string(&path).expect("the config is still at the target"),
                "default_profile = \"old\"\n",
                "the target changed before the rename"
            );
            // A later write replaces the file whether or not the crash
            // left a temporary file beside it.
            write_bytes(&path, b"default_profile = \"old\"\n", &profile_file("new"))
                .expect("write after the crash");
            assert_eq!(fs::read(&path).expect("read"), profile_file("new"));
        }
    });
}

#[test]
fn a_directory_sync_failure_is_reported_after_the_new_config_is_in_place() {
    let (_dir, path, result) = write_failing_at(PublishStep::DirSync);
    assert_eq!(
        result.expect_err("injected failure"),
        "cannot sync configuration directory"
    );
    assert_eq!(fs::read(&path).expect("read"), profile_file("new"));
}

#[test]
fn write_creates_a_persistent_lock_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    write_bytes(&path, b"", &profile_file("first")).expect("write");

    let lock = dir.path().join("config.toml.lock");
    let mode = fs::metadata(&lock)
        .expect("lock metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "mode = {mode:#o}");
}

#[test]
fn write_waits_for_the_configuration_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let result = dir.path().join("result.txt");
    let held = lock_config(&path).expect("hold lock");
    let mut child = config_lock_child(&path, &result);

    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        assert!(
            child.try_wait().expect("poll child").is_none(),
            "writer must wait for the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(held);
    let status = child.wait().expect("wait child");
    assert!(status.success(), "child status = {status}");
    assert_eq!(fs::read_to_string(&result).expect("result"), "ok");
    assert_eq!(fs::read(&path).expect("read"), profile_file("child"));
}

#[test]
fn write_serializes_stale_writers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    let mut first_child = config_lock_child(&path, &first);
    let mut second_child = config_lock_child(&path, &second);

    let first_status = first_child.wait().expect("first wait");
    let second_status = second_child.wait().expect("second wait");
    assert!(first_status.success(), "first status = {first_status}");
    assert!(second_status.success(), "second status = {second_status}");
    let mut results = [
        fs::read_to_string(&first).expect("first result"),
        fs::read_to_string(&second).expect("second result"),
    ];
    results.sort();
    assert_eq!(
        results,
        [
            "err:the configuration changed on disk; rerun to apply this change".to_string(),
            "ok".to_string(),
        ]
    );
    assert_eq!(fs::read(&path).expect("read"), profile_file("child"));
}

#[test]
fn write_serializes_stale_threads_in_one_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let held = CONFIG_WRITE_LOCK.lock().expect("hold process lock");
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        started_tx.send(()).expect("started");
        let result = write_bytes(&writer_path, b"", &profile_file("thread"));
        done_tx.send(result).expect("done");
    });

    started_rx.recv().expect("writer started");
    thread::sleep(Duration::from_millis(50));
    assert!(
        done_rx.try_recv().is_err(),
        "writer must wait for the process lock"
    );
    drop(held);
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("writer finished")
        .expect("write succeeds after unlock");
    writer.join().expect("writer thread");
    assert_eq!(fs::read(&path).expect("read"), profile_file("thread"));
}

fn config_lock_child(path: &Path, result: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().expect("current exe"))
        .arg("--exact")
        .arg("config::tests::config_lock_child_entry")
        .arg("--nocapture")
        .env("OTTO_CONFIG_LOCK_CHILD_PATH", path)
        .env("OTTO_CONFIG_LOCK_CHILD_RESULT", result)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child")
}

#[test]
fn config_lock_child_entry() {
    let Some(path) = std::env::var_os("OTTO_CONFIG_LOCK_CHILD_PATH").map(PathBuf::from) else {
        return;
    };
    let result =
        PathBuf::from(std::env::var_os("OTTO_CONFIG_LOCK_CHILD_RESULT").expect("result path"));
    let text = match write_bytes(&path, b"", &profile_file("child")) {
        Ok(()) => "ok".to_string(),
        Err(error) => format!("err:{error}"),
    };
    fs::write(result, text).expect("write result");
}

#[test]
fn write_backs_up_the_replaced_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let original = "default_profile = \"old\"\n# a comment worth recovering\n";
    let path = write(dir.path(), "config.toml", original);

    write_bytes(&path, original.as_bytes(), &profile_file("new")).expect("write");

    let backups = backups(dir.path());
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        fs::read_to_string(&backups[0]).expect("read backup"),
        original
    );
    let mode = fs::metadata(&backups[0])
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "mode = {mode:#o}");
}

#[test]
fn write_writes_no_backup_when_there_was_no_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    write_bytes(&path, b"", &profile_file("first")).expect("write");

    assert!(backups(dir.path()).is_empty());
}

/// Backups are pruned by name, so the names of writes that land in the
/// same millisecond have to sort in write order like every other pair.
#[test]
fn write_keeps_the_newest_backups_of_a_shared_millisecond() {
    let dir = tempfile::tempdir().expect("tempdir");
    let directory = dir.path().join("backups");
    fs::create_dir_all(&directory).expect("create backups");
    let stamp = "20260923T101112.500Z";
    let written: Vec<String> = (0..13)
        .map(|attempt| {
            let name = backup_name(stamp, attempt);
            write(&directory, &name, &format!("p{attempt}"));
            name
        })
        .collect();

    prune(&directory);

    let kept: Vec<String> = backups(dir.path())
        .iter()
        .map(|path| {
            path.file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        kept,
        written[3..],
        "prune must drop the three oldest, not the newest"
    );
}

#[test]
fn successive_backups_do_not_reuse_pruned_names_in_one_millisecond() {
    let dir = tempfile::tempdir().expect("tempdir");
    for index in 0..13 {
        back_up_with_stamp(
            dir.path(),
            format!("p{index}").as_bytes(),
            "20260923T101112.500Z",
        )
        .expect("backup");
    }
    let kept = backups(dir.path());
    assert_eq!(kept.len(), BACKUP_LIMIT);
    assert_eq!(fs::read_to_string(&kept[0]).expect("oldest"), "p3");
    assert_eq!(fs::read_to_string(&kept[9]).expect("newest"), "p12");
}

#[test]
fn write_keeps_only_the_most_recent_backups() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    for index in 0..13 {
        let current = fs::read(&path).unwrap_or_default();
        write_bytes(&path, &current, &profile_file(&format!("p{index}"))).expect("write");
    }

    let backups = backups(dir.path());
    assert_eq!(backups.len(), 10, "{backups:?}");
    let oldest = fs::read_to_string(&backups[0]).expect("read oldest");
    let newest = fs::read_to_string(&backups[9]).expect("read newest");
    assert!(oldest.contains("\"p2\""), "{oldest}");
    assert!(newest.contains("\"p11\""), "{newest}");
}

#[test]
fn set_default_profile_backs_up_the_replaced_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let original = "default_profile = \"old\"\n[profiles.old]\nprovider = \"chatgpt\"\n[profiles.new]\nprovider = \"chatgpt\"\n";
    let path = write(dir.path(), "config.toml", original);

    set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

    let backups = backups(dir.path());
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        fs::read_to_string(&backups[0]).expect("read backup"),
        original
    );
}

#[test]
fn default_path_uses_home_dir() {
    assert_eq!(
        default_path_for_home(Some("/home/u")),
        PathBuf::from("/home/u/.config/otto/config.toml")
    );
}

#[test]
fn default_path_falls_back_to_literal_tilde_when_home_is_absent_or_empty() {
    assert_eq!(
        default_path_for_home(None),
        PathBuf::from("~/.config/otto/config.toml")
    );
    assert_eq!(
        default_path_for_home(Some("")),
        PathBuf::from("~/.config/otto/config.toml")
    );
}

#[test]
fn load_returns_empty_file_for_missing_default_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("config.toml");
    let file = load_impl(&default, &default).expect("load");
    assert_eq!(file, File::default());
}

#[test]
fn load_rejects_missing_explicit_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("missing.toml");
    let default = dir.path().join("elsewhere.toml");
    let err = load_impl(&path, &default).unwrap_err();
    assert!(err.is_not_found(), "{err}");
}

#[test]
fn load_required_never_treats_missing_path_as_implicit_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let err = load_required(&path).unwrap_err();
    assert!(err.is_not_found(), "{err}");
}

#[test]
fn load_required_decodes_without_consulting_default_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write(
        dir.path(),
        "config.toml",
        "default_profile = \"local\"\n[profiles.local]\nprovider = \"openai-compatible\"\n",
    );
    let file = load_required(&path).expect("load_required");
    assert_eq!(file.default_profile, "local");
    assert_eq!(file.profiles["local"].provider, "openai-compatible");
}

#[test]
fn load_rejects_unknown_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write(
        dir.path(),
        "config.toml",
        "default_profile = \"local\"\nunknown = true\n",
    );
    let err = load_required(&path).unwrap_err();
    assert!(err.to_string().contains("unknown"), "{err}");
}

#[test]
fn write_creates_the_directory_and_restricts_permissions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("config.toml");
    write_bytes(&path, b"", &profile_file("local")).expect("write");

    let reloaded = load_required(&path).expect("load_required");
    assert_eq!(reloaded.default_profile, "local");

    let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode = {mode:#o}");
}

#[test]
fn set_default_profile_updates_existing_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write(
        dir.path(),
        "config.toml",
        "default_profile = \"old\"\n[profiles.old]\nprovider = \"openai-compatible\"\nmodel = \"old-model\"\nbase_url = \"https://old.example/v1\"\napi_key_env = \"OLD_KEY\"\n[profiles.new]\nprovider = \"chatgpt\"\nmodel = \"gpt-5-codex\"\n",
    );
    set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

    let content = fs::read_to_string(&path).expect("read back");
    assert!(content.contains("default_profile = \"new\""));
    assert!(!content.contains("default_profile = \"old\""));

    let file = load_required(&path).expect("load_required");
    assert_eq!(file.default_profile, "new");
    assert_eq!(file.profiles["old"].model, "old-model");
    assert_eq!(file.profiles["new"].provider, "chatgpt");
}

#[test]
fn set_default_profile_changes_only_the_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let original = "# settings\n\ndefault_profile = \"old\" # picked by /model\n\n# profiles below\n[profiles.old]\nprovider = \"chatgpt\"\n[profiles.new]\nprovider = \"chatgpt\"\n";
    let path = write(dir.path(), "config.toml", original);

    set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        original.replace("\"old\" #", "\"new\" #")
    );
}

#[test]
fn set_profile_thinking_changes_only_the_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let original = "default_profile = \"a\"\n\n# the main profile\n[profiles.a]\nprovider = \"chatgpt\"\nmodel = \"m\" # fast\nthinking = \"low\"   # cheap\n\n# a second one\n[profiles.b]\nprovider = \"chatgpt\"\n\n# trailing note\n";
    let path = write(dir.path(), "config.toml", original);

    set_profile_thinking_file(&path, "a", "high").expect("set thinking");
    let replaced = original.replace("\"low\"   #", "\"high\"   #");
    assert_eq!(fs::read_to_string(&path).expect("read back"), replaced);

    set_profile_thinking_file(&path, "b", "max").expect("insert thinking");
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        replaced.replace(
            "[profiles.b]\nprovider = \"chatgpt\"\n",
            "[profiles.b]\nprovider = \"chatgpt\"\nthinking = \"max\"\n"
        )
    );
}

#[test]
fn set_default_profile_inserts_missing_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write(
        dir.path(),
        "config.toml",
        "[profiles.new]\nprovider = \"chatgpt\"\nmodel = \"gpt-5-codex\"\n",
    );
    set_default_profile_file_impl(&path, &default, "new").expect("set_default_profile_file");

    let content = fs::read_to_string(&path).expect("read back");
    assert!(
        content.starts_with("default_profile = \"new\"\n"),
        "{content:?}"
    );
}

#[test]
fn set_default_profile_rejects_unknown_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write(
        dir.path(),
        "config.toml",
        "[profiles.known]\nprovider = \"chatgpt\"\nmodel = \"gpt-5-codex\"\n",
    );
    let err = set_default_profile_file_impl(&path, &default, "missing").unwrap_err();
    assert!(
        err.to_string().contains("profile \"missing\" not found"),
        "{err}"
    );
}

#[test]
fn set_default_profile_rejects_empty_profile_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = dir.path().join("config.toml");
    let err = set_default_profile_file_impl(&path, &default, "").unwrap_err();
    assert!(err.to_string().contains("missing profile"), "{err}");
}

#[test]
fn profile_change_preview_commits_a_redacted_byte_preserving_field_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let original = "# retained\ndefault_profile = \"local\"\n[profiles.local]\nprovider = \"openai-compatible\"\nmodel = \"old\" # comment\nbase_url = \"https://example.test/v1\"\napi_key_env = \"LOCAL_KEY\"\n";
    let path = write(dir.path(), "config.toml", original);
    let preview = preview_profile_change(
        &path,
        &ProfileChange::SetField {
            profile: "local".into(),
            field: ProfileField::Model,
            value: "new".into(),
        },
    )
    .expect("preview");

    assert_eq!(preview.original, original.as_bytes());
    assert_eq!(
        String::from_utf8(preview.updated.clone()).expect("utf8"),
        original.replace("\"old\" #", "\"new\" #")
    );
    assert_eq!(preview.metadata.operation, "set profile field");
    assert!(!preview.redacted_diff.contains("new"));
    commit_profile_change(&path, &preview).expect("commit");
    assert_eq!(fs::read(&path).expect("read"), preview.updated);
}

#[test]
fn profile_change_rejects_invalid_names_and_provider_specific_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write(
        dir.path(),
        "config.toml",
        "default_profile = \"chat\"\n[profiles.chat]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n",
    );
    let invalid_name = preview_profile_change_impl(
        &path,
        &default,
        &ProfileChange::SetDefault {
            profile: "bad.name".into(),
        },
    )
    .expect_err("invalid name");
    assert!(invalid_name.to_string().contains("invalid profile name"));

    let chatgpt_base_url = preview_profile_change_impl(
        &path,
        &default,
        &ProfileChange::SetField {
            profile: "chat".into(),
            field: ProfileField::BaseUrl,
            value: "https://example.test/v1".into(),
        },
    )
    .expect_err("chatgpt base url");
    assert!(
        chatgpt_base_url
            .to_string()
            .contains("chatgpt does not support")
    );

    let missing_base_url = preview_profile_change_impl(
        &path,
        &default,
        &ProfileChange::Create {
            profile: "local".into(),
            provider: ProfileProvider::OpenAiCompatible,
            model: "model".into(),
            thinking: String::new(),
            base_url: None,
            api_key_env: None,
        },
    )
    .expect_err("openai-compatible needs base url");
    assert!(missing_base_url.to_string().contains("missing base_url"));
}

#[test]
fn profile_change_rejects_default_and_last_profile_removal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write(
        dir.path(),
        "config.toml",
        "default_profile = \"only\"\n[profiles.only]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n",
    );
    let last = preview_profile_change_impl(
        &path,
        &default,
        &ProfileChange::Remove {
            profile: "only".into(),
        },
    )
    .expect_err("last profile");
    assert!(last.to_string().contains("last profile"));

    fs::write(
            &path,
            "default_profile = \"only\"\n[profiles.only]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n[profiles.other]\nprovider = \"chatgpt\"\nmodel = \"gpt\"\n",
        )
        .expect("rewrite fixture");
    let default_profile = preview_profile_change_impl(
        &path,
        &default,
        &ProfileChange::Remove {
            profile: "only".into(),
        },
    )
    .expect_err("default profile");
    assert!(default_profile.to_string().contains("default profile"));
}
