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
            .and_then(|path| otto::workspace::read(&path).map_err(|error| error.to_string())),
        "fork" => otto::workspace::fork(Path::new(cwd))
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
        assert_eq!(
            run(
                &["fork".into(), "--cwd".into(), cwd],
                &mut output,
                &mut errors
            ),
            0
        );
        assert!(dir.path().join(otto::workspace::MARKER).is_file());
        assert!(errors.is_empty());
        assert_eq!(run(&["bad".into()], &mut output, &mut errors), 2);
    }
}
