use super::*;
use std::io::Cursor;

fn candidate(id: &str, name: &str, archived: bool) -> DeleteCandidate {
    DeleteCandidate {
        id: id.into(),
        name: name.into(),
        path: format!("/{id}.jsonl"),
        archived,
    }
}

#[test]
fn exact_ids_take_precedence_over_names_and_duplicates_fail_closed() {
    let sessions = vec![
        candidate("id", "title", false),
        candidate("other", "id", true),
    ];
    assert_eq!(resolve(&sessions, "id", false).unwrap().id, "id");
    assert_eq!(resolve(&sessions, "id", true).unwrap().id, "id");
    assert!(resolve(&sessions, "tit", false).is_err());
    assert!(resolve(&sessions, "TITLE", false).is_err());
    assert!(
        resolve(&sessions, "title", true)
            .unwrap_err()
            .contains("--force")
    );
    assert_eq!(resolve(&sessions, "title", false).unwrap().id, "id");
    let mut duplicate_ids = sessions;
    let mut duplicate = candidate("id", "third", true);
    duplicate.path = "/archive/id.jsonl".into();
    duplicate_ids.push(duplicate);
    for force in [false, true] {
        assert!(
            resolve(&duplicate_ids, "id", force)
                .unwrap_err()
                .contains("ambiguous")
        );
    }
    let duplicate_names = vec![
        candidate("one", "same", false),
        candidate("two", "same", true),
    ];
    assert!(
        resolve(&duplicate_names, "same", false)
            .unwrap_err()
            .contains("ambiguous")
    );
}

struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    workspace: String,
    storage: otto::runtime::storage::Storage,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(temp.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let root = temp.path().join("sessions");
        Self {
            temp,
            root,
            workspace,
            storage: Default::default(),
        }
    }

    fn create(&self, id: &str, name: &str) -> Arc<dyn otto::session::SessionHandle> {
        let session = self
            .storage
            .sessions()
            .create_lazy(
                &self.root,
                Header {
                    version: CURRENT_VERSION,
                    id: id.into(),
                    workspace: self.workspace.clone(),
                    created_at: chrono::Utc::now(),
                    provider: "openai-compatible".into(),
                    profile: "local".into(),
                    model: "test-model".into(),
                },
                None,
            )
            .unwrap();
        session.rename(name).unwrap();
        session
    }

    fn invoke(&self, target: &str, force: bool, answer: &str) -> (i32, String, String) {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = run(
            self.storage.sessions(),
            &self.root,
            &self.workspace,
            target,
            force,
            &mut Cursor::new(answer),
            &mut stdout,
            &mut stderr,
            str::to_owned,
        );
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }
}

#[test]
fn confirmation_shows_identity_and_defaults_to_no() {
    let fixture = Fixture::new();
    let session = fixture.create("confirm-id", "my session");
    let path = session.path();
    session.close().unwrap();
    for answer in ["", "\n", "n\n", "sure\n"] {
        let (code, output, error) = fixture.invoke("my session", false, answer);
        assert_eq!(code, 0, "{error}");
        for expected in [
            "ID: confirm-id",
            "Name: my session",
            &path,
            "cannot be undone",
            "[y/N]",
            "Cancelled",
        ] {
            assert!(output.contains(expected), "missing {expected}: {output}");
        }
        assert!(Path::new(&path).exists());
    }
    let (code, output, error) = fixture.invoke("my session", false, "yes\n");
    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Permanently deleted session: confirm-id"));
    assert!(!Path::new(&path).exists());
}

#[test]
fn force_id_deletes_active_and_archived_without_confirmation() {
    for archived in [false, true] {
        let fixture = Fixture::new();
        let session = fixture.create("force-id", "title");
        let mut path = session.path();
        session.close().unwrap();
        if archived {
            path = fixture
                .storage
                .sessions()
                .archive(&fixture.root, &fixture.workspace, &path)
                .unwrap()
                .path;
        }
        let (code, output, error) = fixture.invoke("force-id", true, "");
        assert_eq!(code, 0, "{error}");
        assert!(!output.contains("[y/N]"));
        assert!(output.contains("Permanently deleted session: force-id"));
        assert!(!Path::new(&path).exists());
    }
}

#[test]
fn ambiguity_and_force_names_never_delete() {
    let fixture = Fixture::new();
    let first = fixture.create("first-id", "same");
    let first_path = first.path();
    first.close().unwrap();
    let archived_path = fixture
        .storage
        .sessions()
        .archive(&fixture.root, &fixture.workspace, &first_path)
        .unwrap()
        .path;
    let second = fixture.create("second-id", "same");
    let second_path = second.path();
    second.close().unwrap();
    for (target, force, expected) in [
        ("same", false, "ambiguous"),
        ("same", true, "--force"),
        ("sam", false, "not found"),
    ] {
        let (code, output, error) = fixture.invoke(target, force, "yes\n");
        assert_eq!(code, 1);
        assert!(error.contains(expected), "{error}");
        assert!(output.is_empty());
        assert!(Path::new(&archived_path).exists());
        assert!(Path::new(&second_path).exists());
    }
}

#[test]
fn archived_exact_name_can_be_confirmed() {
    let fixture = Fixture::new();
    let session = fixture.create("archived-name-id", "archived name");
    let path = session.path();
    session.close().unwrap();
    let archived = fixture
        .storage
        .sessions()
        .archive(&fixture.root, &fixture.workspace, &path)
        .unwrap()
        .path;
    let (code, output, error) = fixture.invoke("archived name", false, "y\n");
    assert_eq!(code, 0, "{error}");
    assert!(output.contains(&archived));
    assert!(!Path::new(&archived).exists());
}

#[test]
fn live_lease_is_refused_without_deletion() {
    use otto::session::lease::{Lease, create_lease_dir};
    let fixture = Fixture::new();
    let session = fixture.create("leased-id", "leased");
    let path = session.path();
    session.close().unwrap();
    create_lease_dir(Path::new(&path), 30).unwrap();
    let (lease, _) = Lease::acquire(Path::new(&path)).unwrap();
    let (code, output, error) = fixture.invoke("leased-id", true, "");
    assert_eq!(code, 1);
    assert!(
        error.contains("stop the session's running prompt/process"),
        "{error}"
    );
    assert!(!output.contains("Permanently deleted"));
    assert!(Path::new(&path).exists());
    lease.release().unwrap();
}

#[test]
fn busy_and_io_failures_are_actionable_and_not_successes() {
    let fixture = Fixture::new();
    let session = fixture.create("busy-id", "busy");
    let path = session.path();
    let (code, output, error) = fixture.invoke("busy-id", true, "");
    assert_eq!(code, 1);
    assert!(
        error.contains("stop the session's running prompt/process"),
        "{error}"
    );
    assert!(!output.contains("Permanently deleted"));
    assert!(Path::new(&path).exists());
    session.close().unwrap();
    std::fs::write(fixture.temp.path().join("blocker"), b"not a directory").unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = run(
        fixture.storage.sessions(),
        &fixture.temp.path().join("blocker"),
        &fixture.workspace,
        "busy-id",
        true,
        &mut Cursor::new(""),
        &mut stdout,
        &mut stderr,
        str::to_owned,
    );
    assert_eq!(code, 1);
    let error = String::from_utf8(stderr).unwrap();
    assert!(error.contains("Check storage permissions"), "{error}");
    assert!(stdout.is_empty());
    assert!(Path::new(&path).exists());
}
