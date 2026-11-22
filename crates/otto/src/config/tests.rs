use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use otto_faults::publish::FaultyPublishOps;
use otto_faults::{Fault, FaultError, FaultPlan, Rule, Trigger};

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

fn default_model_file(name: &str) -> Vec<u8> {
    format!("default_model = \"chatgpt/{name}\"\n").into_bytes()
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
    let concurrent = "default_model = \"written by another otto\"\n";
    let path = write(dir.path(), "config.toml", concurrent);

    let error = write_bytes(
        &path,
        b"what this process read",
        &default_model_file("mine"),
    )
    .expect_err("stale write");

    assert!(error.to_string().contains("changed on disk"), "{error}");
    assert_eq!(fs::read_to_string(&path).expect("read"), concurrent);
    assert!(backups(dir.path()).is_empty());
}

fn write_failing_at(step: PublishStep) -> (tempfile::TempDir, PathBuf, Result<(), &'static str>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write(dir.path(), "config.toml", "default_model = \"old\"\n");
    let plan = FaultPlan::new(vec![Rule::new(
        step,
        Trigger::Always,
        Fault::Before(FaultError::Os(libc::EIO)),
    )]);
    let result = write_bytes_with(
        &path,
        b"default_model = \"old\"\n",
        &default_model_file("new"),
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
            "default_model = \"old\"\n",
            "{step:?}"
        );
        assert!(temporary_files(dir.path()).is_empty(), "{step:?}");
    }
}

#[test]
fn a_crash_before_the_config_rename_keeps_the_previous_config_at_the_target() {
    use otto_faults::crash::{self, CRASH_EXIT, Role};
    crash::run(crash::name_of(&|| {}), |role| match role {
        Role::Child(dir) => {
            let path = write(dir, "config.toml", "default_model = \"old\"\n");
            let plan = FaultPlan::new(vec![Rule::new(
                PublishStep::Rename,
                Trigger::Always,
                Fault::Exit,
            )]);
            let _ = write_bytes_with(
                &path,
                b"default_model = \"old\"\n",
                &default_model_file("new"),
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
                "default_model = \"old\"\n",
                "the target changed before the rename"
            );
            // A later write replaces the file whether or not the crash
            // left a temporary file beside it.
            write_bytes(
                &path,
                b"default_model = \"old\"\n",
                &default_model_file("new"),
            )
            .expect("write after the crash");
            assert_eq!(fs::read(&path).expect("read"), default_model_file("new"));
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
    assert_eq!(fs::read(&path).expect("read"), default_model_file("new"));
}

#[test]
fn write_creates_a_persistent_lock_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    write_bytes(&path, b"", &default_model_file("first")).expect("write");

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
    assert_eq!(fs::read(&path).expect("read"), default_model_file("child"));
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
    assert_eq!(fs::read(&path).expect("read"), default_model_file("child"));
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
        let result = write_bytes(&writer_path, b"", &default_model_file("thread"));
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
    assert_eq!(fs::read(&path).expect("read"), default_model_file("thread"));
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
    let text = match write_bytes(&path, b"", &default_model_file("child")) {
        Ok(()) => "ok".to_string(),
        Err(error) => format!("err:{error}"),
    };
    fs::write(result, text).expect("write result");
}

#[test]
fn write_backs_up_the_replaced_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let original = "default_model = \"old\"\n# a comment worth recovering\n";
    let path = write(dir.path(), "config.toml", original);

    write_bytes(&path, original.as_bytes(), &default_model_file("new")).expect("write");

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

    write_bytes(&path, b"", &default_model_file("first")).expect("write");

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
        write_bytes(&path, &current, &default_model_file(&format!("p{index}"))).expect("write");
    }

    let backups = backups(dir.path());
    assert_eq!(backups.len(), 10, "{backups:?}");
    let oldest = fs::read_to_string(&backups[0]).expect("read oldest");
    let newest = fs::read_to_string(&backups[9]).expect("read newest");
    assert!(oldest.contains("\"chatgpt/p2\""), "{oldest}");
    assert!(newest.contains("\"chatgpt/p11\""), "{newest}");
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
        "default_model = \"local/m\"\n[providers.local]\ntype = \"openai-compatible\"\nbase_url = \"https://example.test/v1\"\n",
    );
    let file = load_required(&path).expect("load_required");
    assert_eq!(file.default_model, "local/m");
    assert_eq!(file.providers["local"].type_, "openai-compatible");
}

#[test]
fn load_rejects_unknown_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write(
        dir.path(),
        "config.toml",
        "default_model = \"chatgpt/local\"\nunknown = true\n",
    );
    let err = load_required(&path).unwrap_err();
    assert!(err.to_string().contains("unknown"), "{err}");
}

#[test]
fn load_rejects_removed_agent_max_turns_and_web_table_naming_the_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (text, name) in [
        ("[agent]\nmax_turns = 20\n", "max_turns"),
        ("[web]\nallow_hosts = [\"example.com\"]\n", "web"),
    ] {
        let path = write(dir.path(), "config.toml", text);
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains(&format!("`{name}`")), "{err}");
    }
}

#[test]
fn write_creates_the_directory_and_restricts_permissions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("config.toml");
    write_bytes(&path, b"", &default_model_file("local")).expect("write");

    let reloaded = load_required(&path).expect("load_required");
    assert_eq!(reloaded.default_model, "chatgpt/local");

    let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode = {mode:#o}");
}
