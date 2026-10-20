//! Durable workspace-local research artifacts. Each task attempt gets a unique
//! directory; unlike scratch downloads, these files are never deleted on exit.
//! Failed storage adds an explicit handoff warning without changing task status.
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use nix::sys::stat::Mode;

use crate::fsops::{PosixPublish, Publish, PublishMode, publish_file};
use crate::tool::root::Root;

#[derive(Default)]
pub(super) struct Reports {
    tasks: Mutex<HashMap<String, Option<Report>>>,
}

struct Report {
    directory: Root,
    path: String,
    handoff: Option<String>,
}

impl Reports {
    /// Reset on resume: a new attempt must never overwrite an earlier report.
    pub(super) fn prepare(&self, workspace: Option<&Root>, task_id: &str) -> Option<String> {
        let report = workspace.and_then(|root| Report::new(root, task_id).ok());
        let path = report.as_ref().map(|report| report.path.clone());
        self.tasks
            .lock()
            .expect("report lock intact")
            .insert(task_id.to_owned(), report);
        path
    }

    /// Called only for final tasks. The result is already redacted. Keep the
    /// read pointer outside the preview cap so truncation cannot hide it.
    pub(super) fn handoff(&self, task_id: &str, result: &str, overflow: bool) -> String {
        let mut tasks = self.tasks.lock().expect("report lock intact");
        let report = tasks.get_mut(task_id).and_then(Option::as_mut);
        let Some(report) = report else {
            return if overflow {
                "\n[Full report file unavailable; no artifact was saved. The result remains in task/session history.]".to_owned()
            } else {
                String::new()
            };
        };
        if report.handoff.is_none() && overflow {
            report.handoff = Some(match report.save(result) {
                Ok(handoff) => handoff,
                Err(_) => "\n[Full report file unavailable; automatic report storage failed. The result remains in task/session history.]".to_owned(),
            });
        }
        report.handoff.clone().unwrap_or_default()
    }
}

impl Report {
    fn new(workspace: &Root, task_id: &str) -> io::Result<Self> {
        let otto = workspace.create_child_directory(".otto", Mode::S_IRWXU)?;
        let parent = otto.create_child_directory("reports", Mode::S_IRWXU)?;
        // Never replace an existing ignore file, symlink or other entry.
        match publish(&parent, ".gitignore", "*\n") {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let id = crate::auth::random_hex::<16>().map_err(io::Error::other)?;
        let name = format!("{task_id}-{id}");
        let directory = parent.create_new_child_directory(&name, Mode::S_IRWXU)?;
        Ok(Self {
            directory,
            path: format!(".otto/reports/{name}"),
            handoff: None,
        })
    }

    fn save(&self, result: &str) -> io::Result<String> {
        publish(&self.directory, "final.md", result)?;
        let index = heading_index(result);
        let index_hint = if publish(&self.directory, "index.md", &index).is_ok() {
            format!(" Section index: {}/index.md.", self.path)
        } else {
            " Section index unavailable; read the report directly.".to_owned()
        };
        Ok(format!(
            "\n[Full report: {}/final.md ({} UTF-8 bytes, {} lines).{} Use read with \
                path, offset and limit; follow its character_offset cursor with the \
                same line range for more. Report persists after task exit; do not \
                resume a completed task to read it.]",
            self.path,
            result.len(),
            result.lines().count(),
            index_hint,
        ))
    }
}

fn publish(directory: &Root, name: &str, content: &str) -> io::Result<()> {
    let (dir, name) = directory.open_parent(Path::new(name), &|_| Ok(()))?;
    let id = crate::auth::random_hex::<16>().map_err(io::Error::other)?;
    publish_file(
        &dir,
        &Publish {
            name: &name,
            temp: &format!(".report-{id}.tmp"),
            permissions: 0o600,
            mode: PublishMode::Exclusive,
        },
        content.as_bytes(),
        &PosixPublish,
    )
    .map_err(|error| error.source)
}

/// Index Markdown ATX headings by their exact 1-based line in final.md. A
/// report without headings is still fully readable using the read cursor.
fn heading_index(result: &str) -> String {
    let mut index = "# Report index\n\nLine numbers refer to final.md. Use read offset/limit for a section.\n\n".to_owned();
    let mut fence: Option<(char, usize)> = None;
    for (line, text) in result.lines().enumerate() {
        let text = text.trim_start();
        let marker = text.chars().next().unwrap_or_default();
        if marker == '`' || marker == '~' {
            let count = text.chars().take_while(|c| *c == marker).count();
            if count >= 3 {
                match fence {
                    None => fence = Some((marker, count)),
                    Some((open, length))
                        if marker == open && count >= length && text[count..].trim().is_empty() =>
                    {
                        fence = None
                    }
                    _ => {}
                }
                continue;
            }
        }
        if fence.is_some() {
            continue;
        }
        let level = text.bytes().take_while(|byte| *byte == b'#').count();
        if (1..=6).contains(&level) && text[level..].starts_with(' ') {
            index.push_str(&format!("- line {}: {}\n", line + 1, text[level..].trim()));
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn reports_survive_exit_and_task_id_reuse_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let reports = Reports::default();
        let first = reports.prepare(Some(&root), "t1").unwrap();
        let text = format!("# 结论\n{}\n## 证据\nlast section\n", "研究".repeat(3000));
        let hint = reports.handoff("t1", &text, true);
        assert!(hint.contains(&format!("{first}/final.md")));
        assert!(hint.contains(&format!("{} UTF-8 bytes", text.len())));
        assert_eq!(reports.handoff("t1", &text, true), hint);
        let second = reports.prepare(Some(&root), "t1").unwrap();
        assert_ne!(first, second);
        reports.handoff("t1", "second attempt", true);
        drop(reports);
        drop(root);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&first).join("final.md")).unwrap(),
            text
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(second).join("final.md")).unwrap(),
            "second attempt"
        );
        assert!(
            std::fs::read_to_string(dir.path().join(&first).join("index.md"))
                .unwrap()
                .contains("line 3: 证据")
        );
        assert_eq!(
            std::fs::metadata(dir.path().join(first).join("final.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn child_authored_report_survives_scratch_and_reports_drop() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let scratch = super::super::scratch::Scratch::new(&root).unwrap();
        let scratch_path = dir.path().join(scratch.path());
        let reports = Reports::default();
        let path = reports.prepare(Some(&root), "t1").unwrap();
        let report = dir.path().join(&path).join("report.md");
        std::fs::write(&report, "# Findings\nfull research").unwrap();
        assert!(
            reports
                .handoff("t1", "short summary and report path", false)
                .is_empty()
        );
        drop(scratch);
        drop(reports);
        assert!(!scratch_path.exists());
        assert_eq!(
            std::fs::read_to_string(report).unwrap(),
            "# Findings\nfull research"
        );
    }

    #[test]
    fn storage_failures_do_not_claim_a_readable_report_or_touch_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join(".otto")).unwrap();
        let root = Root::open(dir.path()).unwrap();
        let reports = Reports::default();
        assert!(reports.prepare(Some(&root), "t1").is_none());
        assert!(
            reports
                .handoff("t1", "long report", true)
                .contains("unavailable")
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

        std::fs::remove_file(dir.path().join(".otto")).unwrap();
        let path = reports.prepare(Some(&root), "t2").unwrap();
        let target = outside.path().join("keep");
        std::fs::write(&target, "untouched").unwrap();
        symlink(&target, dir.path().join(path).join("final.md")).unwrap();
        let hint = reports.handoff("t2", "must not overwrite", true);
        assert!(hint.contains("storage failed"));
        assert!(!hint.contains("Full report:"));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
    }

    #[test]
    fn index_failure_keeps_the_readable_full_report_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let reports = Reports::default();
        let path = reports.prepare(Some(&root), "t1").unwrap();
        std::fs::write(dir.path().join(&path).join("index.md"), "keep").unwrap();
        let hint = reports.handoff("t1", "complete", true);
        assert!(hint.contains(&format!("{path}/final.md")));
        assert!(hint.contains("index unavailable"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path).join("final.md")).unwrap(),
            "complete"
        );
    }

    #[test]
    fn index_tracks_exact_lines_and_skips_fenced_code_headings() {
        let index = heading_index(
            "# Overview\n```md\n# example\n```\n~~~\n## also code\n~~~\n## 证据\ntext\n### Tail",
        );
        assert!(index.contains("line 1: Overview"));
        assert!(index.contains("line 8: 证据"));
        assert!(index.contains("line 10: Tail"));
        assert!(!index.contains("example"));
        assert!(!index.contains("also code"));
    }
}
