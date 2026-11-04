//! Permanent deletion resolves opaque native repository references, never host paths.
use super::*;
use otto::session::DeleteCandidate;
use otto::session::SessionRepository;

fn resolve<'a>(
    sessions: &'a [DeleteCandidate],
    target: &str,
    force: bool,
) -> Result<&'a DeleteCandidate, String> {
    let ids: Vec<_> = sessions
        .iter()
        .filter(|session| session.id == target)
        .collect();
    if let [session] = ids.as_slice() {
        return Ok(session);
    }
    if !ids.is_empty() {
        return Err("session ID is ambiguous; no session was deleted".into());
    }
    if force {
        return Err(
            "--force requires an exact session ID, not a name; no session was deleted".into(),
        );
    }
    let names: Vec<_> = sessions
        .iter()
        .filter(|session| session.name == target)
        .collect();
    match names.as_slice() {
        [session] => Ok(session),
        [] => Err("session not found; use an exact session ID or exact name".into()),
        _ => {
            Err("session name is ambiguous; use an exact session ID; no session was deleted".into())
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    repository: &dyn SessionRepository,
    root: &Path,
    workspace: &str,
    target: &str,
    force: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    stderr: &mut (dyn Write + Send),
    redact: impl Fn(&str) -> String,
) -> i32 {
    let result = (|| -> Result<(), String> {
        let listed = repository.list_for_delete(root, workspace).map_err(|error| {
            format!("list sessions for deletion: {error}. Check storage permissions and session integrity, then retry; no session was deleted")
        })?;
        let session = resolve(&listed, target, force)?;
        if !force {
            writeln!(stdout, "Permanently delete session?\nID: {}\nName: {}\nPath: {}\nWARNING: This cannot be undone; the session and its history will be permanently removed.\nDelete? [y/N] ",
                redact(&session.id), redact(&session.name), redact(&session.path))
                .map_err(|error| format!("write deletion confirmation: {error}"))?;
            stdout
                .flush()
                .map_err(|error| format!("flush deletion confirmation: {error}"))?;
            let mut answer = String::new();
            stdin
                .read_line(&mut answer)
                .map_err(|error| format!("read deletion confirmation: {error}"))?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                writeln!(stdout, "Cancelled; no session was deleted.")
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
        }
        repository.permanent_delete(root, workspace, &session.path).map_err(|error| {
            format!(concat!(
                "permanent delete failed: {}. If busy or leased, stop the session's running prompt/process and retry; ",
                "for I/O errors, check storage permissions and disk availability before retrying"
            ), error)
        })?;
        writeln!(
            stdout,
            "Permanently deleted session: {}",
            redact(&session.id)
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(error) => fail(stderr, &redact(&error)),
    }
}

#[cfg(test)]
mod tests;
