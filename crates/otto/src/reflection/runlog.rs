//! The per-run Markdown log: what a reflection run proposed and what became
//! of each proposal.
//!
//! `reflection.db` keeps ids, ranges, counts and status only. The log keeps
//! the content, so a user can see what the model proposed, which proposals
//! were dropped and why, and which entries were withheld as external.
//!
//! Secrets: every free-text field passes through the run's redactor, and a
//! field that still matches a rule of [`super::guard::secret`] is replaced by
//! a marker. Transcript text is not logged except through the
//! model's own proposals and, for an invalid answer, the raw answer.
//!
//! Ownership: [`render`] is pure; [`write`] creates one new file per run and
//! never overwrites or removes a file. [`list`] and [`read`] only read: they
//! find a log by comparing directory entry names, never by joining a
//! caller's text onto the directory, and read at most [`MAXIMUM_READ_BYTES`].

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::guard;
use super::store::RunRow;

/// The most bytes of one field written to the log.
const MAXIMUM_FIELD_BYTES: usize = 16 * 1024;
/// The most bytes [`read`] returns, and [`list`] reads of each log.
pub const MAXIMUM_READ_BYTES: u64 = 1024 * 1024;
/// The most runs [`list`] returns.
pub const MAXIMUM_LISTED: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Memory,
    Skill,
}

/// What became of one proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Queued as a memory candidate with this id.
    Queued(String),
    /// Published as a skill approval request with this id.
    Approval(String),
    /// Dropped for this reason, the same name counted in `Report::dropped`.
    Dropped(&'static str),
}

/// One proposal the model returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    /// One line, such as ``create user preference `language` ``.
    pub title: String,
    /// The memory text, or the skill description.
    pub text: String,
    /// The skill body; empty for a memory.
    pub body: String,
    /// The model's stated reason.
    pub reason: String,
    /// The entry ids the proposal cites.
    pub cited: Vec<String>,
    pub outcome: Outcome,
}

/// Everything a log shows about one run.
pub struct Input<'a> {
    pub row: &'a RunRow,
    pub session_path: &'a str,
    /// The error text of a failed run; empty otherwise.
    pub error: &'a str,
    /// The raw model answer of an `invalid_answer` run; empty otherwise.
    pub answer: &'a str,
    pub entries: usize,
    pub truncated: bool,
    /// External entries as (id, tool name or role).
    pub external: &'a [(String, String)],
    /// Why skills were not requested; empty when they were.
    pub skills_note: &'a str,
    pub items: &'a [Item],
    /// The model's retrospective of the slice; empty when it gave none.
    pub retrospective: &'a str,
}

/// Renders the log. `redact` is the run's redactor.
pub fn render(input: &Input<'_>, redact: &dyn Fn(&str) -> String) -> String {
    let row = input.row;
    let line = |text: &str| clean(text, redact).replace('\n', " ");
    let mut out = format!("# Reflection run {}\n\n", row.id);
    out.push_str(&format!("- trigger: {}\n", row.trigger));
    out.push_str(&format!("- session: {}\n", row.session_id));
    out.push_str(&format!("- session file: {}\n", line(input.session_path)));
    out.push_str(&format!("- started: {}\n", row.started_at));
    out.push_str(&format!("- finished: {}\n", row.finished_at));
    out.push_str(&format!("- status: {}\n", row.status.as_str()));
    if !row.detail.is_empty() {
        out.push_str(&format!("- detail: {}\n", line(&row.detail)));
    }
    if !input.error.is_empty() {
        out.push_str(&format!("- error: {}\n", line(input.error)));
    }
    out.push_str(&format!(
        "- entries: {} (after `{}` up to `{}`)\n",
        input.entries, row.from_entry, row.to_entry
    ));
    out.push_str(&format!("- input bytes: {}\n", row.input_bytes));
    out.push_str(&format!(
        "- oldest entries cut to fit max_input_bytes: {}\n",
        if input.truncated { "yes" } else { "no" }
    ));

    // First, because it is what a person opens the log to read. Quoted, so
    // its Markdown cannot forge the sections that follow.
    out.push_str("\n## Retrospective\n\n");
    let retrospective = clean(input.retrospective, redact);
    if retrospective.trim().is_empty() {
        out.push_str("none\n");
    } else {
        for text in retrospective.lines() {
            out.push_str(if text.is_empty() { ">" } else { "> " });
            out.push_str(text);
            out.push('\n');
        }
    }

    out.push_str("\n## External entries\n\n");
    if input.external.is_empty() {
        out.push_str("none\n");
    } else {
        out.push_str("Their content was not shown to the model.\n\n");
        for (id, source) in input.external {
            let source = if source.is_empty() { "unknown" } else { source };
            out.push_str(&format!("- `{id}` ({})\n", line(source)));
        }
    }

    out.push_str("\n## Skills\n\n");
    if input.skills_note.is_empty() {
        out.push_str("Skills were requested from the model.\n");
    } else {
        out.push_str(&format!(
            "Skills were not requested: {}.\n",
            input.skills_note
        ));
    }

    for (heading, kind) in [
        ("Memory proposals", Kind::Memory),
        ("Skill proposals", Kind::Skill),
    ] {
        out.push_str(&format!("\n## {heading}\n\n"));
        let items: Vec<&Item> = input.items.iter().filter(|i| i.kind == kind).collect();
        if items.is_empty() {
            out.push_str("none\n");
        }
        for (index, item) in items.iter().enumerate() {
            out.push_str(&format!("### {}. {}\n\n", index + 1, line(&item.title)));
            out.push_str(&format!("- outcome: {}\n", outcome(&item.outcome)));
            let cited: Vec<String> = item.cited.iter().map(|id| line(id)).collect();
            out.push_str(&format!("- cites: {}\n\n", cited.join(", ")));
            for (label, text) in [
                ("text", &item.text),
                ("body", &item.body),
                ("reason", &item.reason),
            ] {
                if !text.is_empty() {
                    out.push_str(&format!("{label}:\n\n{}\n\n", fenced(&clean(text, redact))));
                }
            }
        }
    }

    if !input.answer.is_empty() {
        out.push_str("\n## Raw model answer\n\n");
        out.push_str(&fenced(&clean(input.answer, redact)));
        out.push('\n');
    }
    out
}

fn outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Queued(id) => format!("queued as candidate {id}"),
        Outcome::Approval(id) => format!("approval request {id}"),
        Outcome::Dropped(reason) => format!("dropped: {reason}"),
    }
}

/// Redacts `text`, then withholds it if a secret rule still matches.
pub(super) fn clean(text: &str, redact: &dyn Fn(&str) -> String) -> String {
    let redacted = redact(text);
    match guard::secret(&redacted) {
        Some(rule) => format!("(withheld: {rule})"),
        None => redacted,
    }
}

/// `text` in a code fence one backtick longer than its longest backtick run
/// (at least 3), cut to [`MAXIMUM_FIELD_BYTES`].
fn fenced(text: &str) -> String {
    let mut text = text;
    let mut marker = String::new();
    if text.len() > MAXIMUM_FIELD_BYTES {
        let mut end = MAXIMUM_FIELD_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        marker = format!("\n[truncated: {} bytes omitted]", text.len() - end);
        text = &text[..end];
    }
    let mut longest = 0;
    let mut run = 0;
    for character in text.chars() {
        run = if character == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}\n{text}{marker}\n{fence}")
}

/// Writes `content` to a new file in `dir`, creating `dir` (mode 0700) if
/// needed. The file is `<started as YYYYMMDDTHHMMSSZ>-<first 8 of run id>.md`,
/// mode 0600, and is never overwritten.
pub fn write(dir: &Path, started_at: &str, run_id: &str, content: &str) -> Result<PathBuf, String> {
    let started = chrono::DateTime::parse_from_rfc3339(started_at)
        .map_err(|error| format!("run start time: {error}"))?
        .with_timezone(&chrono::Utc);
    let name = format!(
        "{}-{}.md",
        started.format("%Y%m%dT%H%M%SZ"),
        run_id.chars().take(8).collect::<String>()
    );
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| format!("create {}: {error}", dir.display()))?;
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

/// One run log as `/reflect list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    /// The id `/reflect show` takes: the first 8 characters of the run id.
    pub id: String,
    pub path: PathBuf,
    pub started: String,
    pub trigger: String,
    pub status: String,
    /// The first line of the retrospective; empty when it has none.
    pub headline: String,
}

/// The log file name's parts: `<YYYYMMDDTHHMMSSZ>-<id>.md`.
fn parse_name(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".md")?;
    let (time, id) = stem.split_once('-')?;
    (time.len() == 16
        && !id.is_empty()
        && id.len() <= 8
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    .then_some(id)
}

/// The newest `limit` (at most [`MAXIMUM_LISTED`]) run logs in `dir`, newest
/// first. A missing directory has no runs.
pub fn list(dir: &Path, limit: usize) -> Result<Vec<Summary>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", dir.display())),
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| parse_name(name).is_some())
        .collect();
    // The name starts with the UTC start time, so it sorts by time.
    names.sort_unstable_by(|a, b| b.cmp(a));
    names.truncate(limit.min(MAXIMUM_LISTED));
    Ok(names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            let content = read_bounded(&path).unwrap_or_default();
            summarize(parse_name(name).unwrap_or_default(), path, &content)
        })
        .collect())
}

/// The run log whose id is `id` (as [`Summary::id`] shows it), with its
/// content, or `None` when `dir` holds no such log.
pub fn read(dir: &Path, id: &str) -> Result<Option<(PathBuf, String)>, String> {
    let wanted = id.trim();
    if wanted.is_empty() {
        return Ok(None);
    }
    for summary in list(dir, MAXIMUM_LISTED)? {
        if summary.id == wanted {
            let content = read_bounded(&summary.path)?;
            return Ok(Some((summary.path, content)));
        }
    }
    // Older than the listed window: look the name up directly.
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", dir.display())),
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if parse_name(&name) == Some(wanted) && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let path = dir.join(&name);
            let content = read_bounded(&path)?;
            return Ok(Some((path, content)));
        }
    }
    Ok(None)
}

/// The list as text, one run per line, for every frontend.
pub fn list_text(runs: &[Summary]) -> String {
    if runs.is_empty() {
        return "No reflection runs yet.".to_owned();
    }
    let mut text = "Reflection runs, newest first (/reflect show <id>):".to_owned();
    for run in runs {
        let started = chrono::DateTime::parse_from_rfc3339(&run.started)
            .map(|time| {
                time.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|_| "-".to_owned());
        text.push_str(&format!(
            "\n  {}  {started}  {}  {}",
            run.id, run.trigger, run.status
        ));
        if !run.headline.is_empty() {
            text.push_str("  ");
            text.push_str(&run.headline.chars().take(80).collect::<String>());
        }
    }
    text
}

fn read_bounded(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAXIMUM_READ_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Reads the header lines and the retrospective's first line that [`render`]
/// wrote.
fn summarize(id: &str, path: PathBuf, content: &str) -> Summary {
    let field = |name: &str| {
        content
            .lines()
            .take_while(|line| !line.starts_with("## "))
            .find_map(|line| line.strip_prefix(&format!("- {name}: ")))
            .unwrap_or_default()
            .to_owned()
    };
    let headline = content
        .split_once("\n## Retrospective\n\n")
        .and_then(|(_, rest)| rest.lines().next())
        .and_then(|line| line.strip_prefix("> "))
        .unwrap_or_default()
        .to_owned();
    Summary {
        id: id.to_owned(),
        started: field("started"),
        trigger: field("trigger"),
        status: field("status"),
        headline,
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_is_longer_than_any_backtick_run_in_the_text() {
        assert_eq!(fenced("a"), "```\na\n```");
        assert_eq!(fenced("x ```` y"), "`````\nx ```` y\n`````");
    }

    #[test]
    fn a_long_field_is_cut_with_a_marker() {
        let long = "é".repeat(MAXIMUM_FIELD_BYTES);
        let out = fenced(&long);
        assert!(out.contains("[truncated: "));
        assert!(out.len() < MAXIMUM_FIELD_BYTES + 100);
    }

    #[test]
    fn a_field_with_a_secret_is_withheld_and_absent_from_the_log() {
        let row = RunRow {
            id: "run-1".into(),
            session_id: "s".into(),
            trigger: "manual".into(),
            started_at: "2026-10-04T01:02:03.000000000Z".into(),
            finished_at: "2026-10-04T01:02:04.000000000Z".into(),
            status: crate::reflection::Status::Ok,
            from_entry: String::new(),
            to_entry: "e2".into(),
            input_bytes: 10,
            truncated: false,
            memories_proposed: 0,
            memories_dropped: 0,
            skills_written: 0,
            detail: String::new(),
        };
        let item = Item {
            kind: Kind::Memory,
            title: "create user fact `k`".into(),
            text: "key AKIAABCDEFGHIJKLMNOP here".into(),
            body: "zero\u{200B}width ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".into(),
            reason: "plain reason with ``` inside".into(),
            cited: vec!["e1".into()],
            outcome: Outcome::Dropped("bad_text"),
        };
        let log = render(
            &Input {
                row: &row,
                session_path: "/tmp/s.jsonl",
                error: "",
                answer: "",
                entries: 2,
                truncated: false,
                external: &[("e2".into(), "bash".into())],
                skills_note: "[reflection].skills is false",
                items: &[item],
                retrospective: "",
            },
            &|text| text.to_owned(),
        );
        assert!(log.contains("(withheld: scan_secret_aws_key)"), "{log}");
        assert!(!log.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(
            log.contains("(withheld: scan_secret_github_token)"),
            "{log}"
        );
        assert!(!log.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"));
        assert!(
            log.contains("````\nplain reason with ``` inside\n````"),
            "{log}"
        );
        assert!(log.contains("dropped: bad_text"));
        assert!(log.contains("- `e2` (bash)"));
    }

    #[test]
    fn the_retrospective_comes_first_quoted_and_cleaned() {
        let row = RunRow {
            id: "run-2".into(),
            session_id: "s".into(),
            trigger: "requested".into(),
            started_at: "2026-10-05T01:02:03.000000000Z".into(),
            finished_at: "2026-10-05T01:02:04.000000000Z".into(),
            status: crate::reflection::Status::Ok,
            from_entry: String::new(),
            to_entry: "e1".into(),
            input_bytes: 10,
            truncated: false,
            memories_proposed: 0,
            memories_dropped: 0,
            skills_written: 0,
            detail: String::new(),
        };
        let render_with = |retrospective: &str| {
            render(
                &Input {
                    row: &row,
                    session_path: "/tmp/s.jsonl",
                    error: "",
                    answer: "",
                    entries: 1,
                    truncated: false,
                    external: &[],
                    skills_note: "",
                    items: &[],
                    retrospective,
                },
                &|text| text.replace("hunter2", "[redacted]"),
            )
        };

        let log = render_with("Searched first.\n\n## Memory proposals\nPassword hunter2.");
        let section = log.find("## Retrospective").expect("section");
        assert!(section < log.find("## External entries").expect("external"));
        assert!(
            log.contains("> Searched first.\n>\n> ## Memory proposals\n"),
            "{log}"
        );
        assert_eq!(log.matches("\n## Memory proposals").count(), 1, "{log}");
        assert!(log.contains("> Password [redacted]."));
        assert!(!log.contains("hunter2"));

        let secret = render_with("token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        assert!(
            secret.contains("> (withheld: scan_secret_github_token)"),
            "{secret}"
        );
        assert!(render_with("").contains("## Retrospective\n\nnone\n"));
    }

    #[test]
    fn runs_are_listed_newest_first_and_read_by_id() {
        let dir = tempfile::tempdir().expect("dir");
        let missing = dir.path().join("runs");
        assert_eq!(list(&missing, 10).expect("missing dir"), []);
        assert_eq!(list_text(&[]), "No reflection runs yet.");

        let row = |id: &str, started: &str, trigger: &str| RunRow {
            id: id.into(),
            session_id: "s".into(),
            trigger: trigger.into(),
            started_at: started.into(),
            finished_at: started.into(),
            status: crate::reflection::Status::Ok,
            from_entry: String::new(),
            to_entry: String::new(),
            input_bytes: 0,
            truncated: false,
            memories_proposed: 0,
            memories_dropped: 0,
            skills_written: 0,
            detail: String::new(),
        };
        for (id, started, trigger, retrospective) in [
            ("aaaa1111-run", "2026-10-04T01:00:00Z", "on_work", ""),
            (
                "bbbb2222-run",
                "2026-10-05T02:03:00Z",
                "requested",
                "Searched before reading.\nSecond line.",
            ),
        ] {
            let row = row(id, started, trigger);
            let content = render(
                &Input {
                    row: &row,
                    session_path: "/s.jsonl",
                    error: "",
                    answer: "",
                    entries: 1,
                    truncated: false,
                    external: &[],
                    skills_note: "",
                    items: &[],
                    retrospective,
                },
                &|text| text.to_owned(),
            );
            write(&missing, started, id, &content).expect("write");
        }
        std::fs::write(missing.join("notes.txt"), "not a log").expect("stray file");
        std::fs::create_dir(missing.join("20261006T000000Z-cccc3333.md")).expect("stray dir");

        let runs = list(&missing, 10).expect("list");
        let ids: Vec<&str> = runs.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["bbbb2222", "aaaa1111"]);
        assert_eq!(runs[0].trigger, "requested");
        assert_eq!(runs[0].status, "ok");
        assert_eq!(runs[0].headline, "Searched before reading.");
        assert_eq!(runs[1].headline, "");
        assert_eq!(list(&missing, 1).expect("limit").len(), 1);
        let text = list_text(&runs);
        assert!(
            text.contains(
                "\n  bbbb2222  2026-10-05 02:03  requested  ok  Searched before reading."
            ),
            "{text}"
        );

        let (path, content) = read(&missing, "aaaa1111").expect("read").expect("found");
        assert!(path.ends_with("20261004T010000Z-aaaa1111.md"));
        assert!(content.starts_with("# Reflection run aaaa1111-run"));
        for unknown in ["", "zzzz9999", "../runs", "cccc3333", "notes"] {
            assert_eq!(read(&missing, unknown).expect("read"), None, "{unknown}");
        }
    }
}
