//! Human-only inspection and explicit forking of local workspace identity.

use std::io::Write;
use std::path::Path;

pub fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let (command, cwd) = match args {
        [command] => (command.as_str(), "."),
        [command, flag, cwd] if flag == "--cwd" && !cwd.is_empty() => {
            (command.as_str(), cwd.as_str())
        }
        _ => {
            let _ = writeln!(stderr, "usage: otto workspace status|fork [--cwd PATH]");
            return 2;
        }
    };
    let result = match command {
        "status" => std::fs::canonicalize(cwd)
            .map_err(|error| error.to_string())
            .and_then(|path| {
                otto_session::workspace::read(&path).map_err(|error| error.to_string())
            }),
        "fork" => otto_session::workspace::fork(Path::new(cwd))
            .map(Some)
            .map_err(|error| error.to_string()),
        _ => {
            let _ = writeln!(stderr, "usage: otto workspace status|fork [--cwd PATH]");
            return 2;
        }
    };
    match result {
        Ok(Some(identity)) => {
            let _ = writeln!(stdout, "Workspace ID: {}", identity.id);
            let _ = writeln!(stdout, "Session key: {}", identity.session_key);
            let _ = writeln!(stdout, "Memory scope: {}", identity.memory_scope);
            0
        }
        Ok(None) => {
            let _ = writeln!(
                stdout,
                "Workspace identity: not initialized (created on startup)"
            );
            0
        }
        Err(_) => {
            let _ = writeln!(
                stderr,
                "workspace identity is invalid, unavailable, or conflicts with configuration"
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_read_only_and_fork_replaces_ownership_without_deleting_state() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        assert_eq!(
            run(
                &["status".into(), "--cwd".into(), cwd.clone()],
                &mut output,
                &mut errors
            ),
            0
        );
        assert!(!dir.path().join(".otto").exists());
        let previous = otto_session::workspace::initialize(dir.path(), "").unwrap();
        let state = dir.path().join(".otto/retained-state");
        std::fs::write(&state, b"synthetic workspace state").unwrap();
        let marker = dir.path().join(otto_session::workspace::MARKER);
        let previous_bytes = std::fs::read(&marker).unwrap();
        output.clear();
        assert_eq!(
            run(
                &["status".into(), "--cwd".into(), cwd.clone()],
                &mut output,
                &mut errors
            ),
            0
        );
        assert_eq!(std::fs::read(&marker).unwrap(), previous_bytes);
        assert_eq!(
            String::from_utf8(output.clone()).unwrap(),
            format!(
                "Workspace ID: {}\nSession key: {}\nMemory scope: {}\n",
                previous.id, previous.session_key, previous.memory_scope
            )
        );
        output.clear();
        assert_eq!(
            run(
                &["fork".into(), "--cwd".into(), cwd],
                &mut output,
                &mut errors
            ),
            0
        );
        let forked = otto_session::workspace::read(dir.path()).unwrap().unwrap();
        assert_ne!(forked.id, previous.id);
        assert_ne!(forked.session_key, previous.session_key);
        assert_ne!(forked.memory_scope, previous.memory_scope);
        assert_eq!(std::fs::read(&state).unwrap(), b"synthetic workspace state");
        assert_eq!(
            String::from_utf8(output.clone()).unwrap(),
            format!(
                "Workspace ID: {}\nSession key: {}\nMemory scope: {}\n",
                forked.id, forked.session_key, forked.memory_scope
            )
        );
        assert!(errors.is_empty());
        assert_eq!(run(&["bad".into()], &mut output, &mut errors), 2);
    }
}
