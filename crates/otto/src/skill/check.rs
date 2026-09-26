//! Automatic skill contract check (experimental).
//!
//! The first time a skill is delegated to as a sub-agent, [`Checker::trigger`]
//! runs in the background: deterministic rules run first
//! ([`context_reference_fires`], [`vague_output_fires`]); if none fires,
//! [`TypeSafeClient`] asks four questions about the skill's `SKILL.md`. Every
//! result is appended to an SQLite database and never overwritten. Verdicts
//! ([`hint_for`]) are computed from the stored answers when displayed, not
//! stored, so a threshold change is visible immediately.
//!
//! See `docs/specs/2026-09-25-skill-contract-check.md`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::skill::frontmatter;
use crate::tool::root::Root;

/// TypeSafe request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default backoff before each of up to 3 retries on 429/529.
const DEFAULT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
const MODEL: &str = "jev-latest";
const TYPESAFE_URL_SUFFIX: &str = "/v1/systemone";
/// The production TypeSafe endpoint's base URL, passed to [`Checker::open`]
/// at the composition root; never read from here.
pub const TYPESAFE_BASE_URL: &str = "https://api.typesafe.ai";

// ---------------------------------------------------------------------------
// Stage 1: deterministic rules
// ---------------------------------------------------------------------------

/// One deterministic rule: `question` is the id of the question it answers
/// "no" to when it fires.
struct Rule {
    id: &'static str,
    question: &'static str,
}

const CONTEXT_REFERENCE_PATTERNS: &[&str] = &[
    "earlier in the conversation",
    "previous message",
    "as discussed",
    "the conversation above",
    "the user's last message",
];

const VAGUE_OUTPUT_PHRASES: &[&str] = &[
    "the result",
    "a result",
    "a summary",
    "the summary",
    "the output",
    "the answer",
];

const RULES: &[Rule] = &[
    Rule {
        id: "context_reference",
        question: "self_contained",
    },
    Rule {
        id: "vague_output",
        question: "output_usable",
    },
];

/// Fires when `body`, case-insensitively, contains a phrase implying the
/// reader needs conversation history not named in `input`.
fn context_reference_fires(body: &str) -> bool {
    let lower = body.to_lowercase();
    CONTEXT_REFERENCE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Fires when `output`, trimmed and lowercased with trailing punctuation
/// stripped, is under 4 words or one of the stock vague phrases.
fn vague_output_fires(output: &str) -> bool {
    let trimmed = output
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .trim();
    let lower = trimmed.to_lowercase();
    VAGUE_OUTPUT_PHRASES.contains(&lower.as_str()) || lower.split_whitespace().count() < 4
}

/// Runs every rule against `body` (the `context_reference` target) and
/// `output` (the `vague_output` target), in [`RULES`] order.
fn fired_rules(body: &str, output: &str) -> Vec<&'static Rule> {
    let mut fired = Vec::new();
    if context_reference_fires(body) {
        fired.push(&RULES[0]);
    }
    if vague_output_fires(output) {
        fired.push(&RULES[1]);
    }
    fired
}

// ---------------------------------------------------------------------------
// Stage 2: TypeSafe questions and hashing
// ---------------------------------------------------------------------------

/// A question's criteria, shaped per its `kind`: a choice question names
/// each option, a noul question names its true/false meaning, and a score
/// question orders its levels from 0. Kept structured, rather than one
/// flattened string, so [`Criteria::to_json`] can emit the wire shape TypeSafe
/// expects and [`questions_sha256`] can hash the real fields.
enum Criteria {
    Choice(&'static [(&'static str, &'static str)]),
    Noul {
        when_true: &'static str,
        when_false: &'static str,
    },
    Score(&'static [&'static str]),
}

impl Criteria {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Choice(options) => options
                .iter()
                .map(|(name, description)| {
                    (name.to_string(), serde_json::Value::from(*description))
                })
                .collect(),
            Self::Noul {
                when_true,
                when_false,
            } => serde_json::json!({"true": when_true, "false": when_false}),
            Self::Score(levels) => levels.iter().copied().collect(),
        }
    }

    fn hash(&self, hasher: &mut Sha256) {
        match self {
            Self::Choice(options) => {
                for (name, description) in *options {
                    hasher.update(name);
                    hasher.update(description);
                }
            }
            Self::Noul {
                when_true,
                when_false,
            } => {
                hasher.update(when_true);
                hasher.update(when_false);
            }
            Self::Score(levels) => {
                for level in *levels {
                    hasher.update(level);
                }
            }
        }
    }
}

struct Question {
    id: &'static str,
    kind: &'static str,
    instructions: &'static str,
    criteria: Criteria,
}

const QUESTIONS: [Question; 4] = [
    Question {
        id: "procedural",
        kind: "choice",
        instructions: "Is the body of this skill a procedure to execute, or reference knowledge to consult?",
        criteria: Criteria::Choice(&[
            (
                "procedural",
                "ordered steps that take the declared input to the declared output.",
            ),
            (
                "knowledge",
                "facts, conventions or guidance without a fixed sequence.",
            ),
            ("mixed", "both, with neither dominant."),
        ]),
    },
    Question {
        id: "self_contained",
        kind: "noul",
        instructions: "A sub-agent starts with an empty context and receives only what the `input` field describes. Can it carry out the body's steps without information from the conversation that delegated to it?",
        criteria: Criteria::Noul {
            when_true: "everything the steps need is named in `input` or in the skill package.",
            when_false: "the steps depend on earlier conversation, the user's recent messages, or files not named in `input`.",
        },
    },
    Question {
        id: "output_usable",
        kind: "noul",
        instructions: "Does the `output` field describe a result the delegating agent can use directly, with its content and shape stated?",
        criteria: Criteria::Noul {
            when_true: "names the content and its form (fields, format, file path).",
            when_false: "vague, such as \"a summary\" or \"the result\".",
        },
    },
    Question {
        id: "body_matches_contract",
        kind: "score",
        instructions: "Do the body's steps read the declared `input` and produce the declared `output`?",
        criteria: Criteria::Score(&["does not match", "partly matches", "matches"]),
    },
];

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `sha256(SKILL.md bytes)`.
fn content_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// `sha256` of the question set and the rule patterns: editing any of them
/// changes the key, so every skill is re-checked on its next delegation.
fn questions_sha256() -> String {
    let mut hasher = Sha256::new();
    for question in &QUESTIONS {
        hasher.update(question.id);
        hasher.update(question.kind);
        hasher.update(question.instructions);
        question.criteria.hash(&mut hasher);
    }
    for rule in RULES {
        hasher.update(rule.id);
        hasher.update(rule.question);
    }
    for pattern in CONTEXT_REFERENCE_PATTERNS {
        hasher.update(pattern);
    }
    for phrase in VAGUE_OUTPUT_PHRASES {
        hasher.update(phrase);
    }
    to_hex(&hasher.finalize())
}

// ---------------------------------------------------------------------------
// TypeSafe wire types and client
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WireQuestion<'a> {
    r#type: &'a str,
    instructions: &'a str,
    criteria: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RequestBody<'a> {
    state: &'a str,
    model: &'a str,
    questions: serde_json::Map<String, serde_json::Value>,
}

/// The response `answers` object, kept exactly as returned (map keyed by
/// question id; each value carries `type` plus type-specific fields such as
/// `noul`, `choice` or `score`). This is also the shape persisted in the
/// store's `result` column.
type AnswerMap = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Deserialize)]
struct ResponseBody {
    model: String,
    answers: AnswerMap,
}

/// A validated TypeSafe answer set: every question in [`QUESTIONS`] is
/// present with a value of the right JSON type.
#[derive(Debug, Clone, PartialEq)]
struct Answers {
    model: String,
    answers: AnswerMap,
}

/// Why a check produced no row. Never carries the API key or the request
/// body.
#[derive(Debug, Clone, PartialEq)]
enum CheckError {
    Status(u16),
    Timeout,
    Transport,
    InvalidResponse,
}

impl CheckError {
    /// The text `/skills` shows as `not checked yet (last attempt: <reason>)`.
    fn reason(&self) -> String {
        match self {
            Self::Status(code) => code.to_string(),
            Self::Timeout => "timeout".to_string(),
            Self::Transport => "transport error".to_string(),
            Self::InvalidResponse => "invalid response".to_string(),
        }
    }
}

fn validate_answers(body: ResponseBody) -> Result<Answers, CheckError> {
    for question in &QUESTIONS {
        let Some(answer) = body.answers.get(question.id) else {
            return Err(CheckError::InvalidResponse);
        };
        if answer.get("type").and_then(serde_json::Value::as_str) != Some(question.kind) {
            return Err(CheckError::InvalidResponse);
        }
        // The type-specific field is named after the question's own kind:
        // a noul answer's number lives under "noul", a choice answer's
        // string under "choice", a score answer's number under "score".
        let well_typed = match question.kind {
            "noul" | "score" => answer
                .get(question.kind)
                .and_then(serde_json::Value::as_f64)
                .is_some(),
            _ => answer
                .get(question.kind)
                .and_then(serde_json::Value::as_str)
                .is_some(),
        };
        if !well_typed {
            return Err(CheckError::InvalidResponse);
        }
    }
    Ok(Answers {
        model: body.model,
        answers: body.answers,
    })
}

/// A TypeSafe HTTP client. `base_url` and `api_key` are injected; nothing
/// here reads the environment.
struct TypeSafeClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    backoff: Vec<Duration>,
}

impl TypeSafeClient {
    fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            backoff: DEFAULT_BACKOFF.to_vec(),
        }
    }

    /// Replaces the retry backoff so tests do not wait seconds for a 429.
    #[cfg(test)]
    fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
    }

    async fn check(&self, state: &str) -> Result<Answers, CheckError> {
        let request = RequestBody {
            state,
            model: MODEL,
            questions: QUESTIONS
                .iter()
                .map(|question| {
                    let wire = WireQuestion {
                        r#type: question.kind,
                        instructions: question.instructions,
                        criteria: question.criteria.to_json(),
                    };
                    (
                        question.id.to_string(),
                        serde_json::to_value(wire).expect("WireQuestion serializes"),
                    )
                })
                .collect(),
        };
        let payload = serde_json::to_vec(&request).map_err(|_| CheckError::InvalidResponse)?;
        let url = format!("{}{TYPESAFE_URL_SUFFIX}", self.base_url);

        let mut attempt = 0usize;
        loop {
            let sent = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .timeout(REQUEST_TIMEOUT)
                .body(payload.clone())
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(error) if error.is_timeout() => return Err(CheckError::Timeout),
                Err(_) => return Err(CheckError::Transport),
            };
            let status = response.status().as_u16();
            if status == 200 {
                let bytes = response.bytes().await.map_err(|_| CheckError::Transport)?;
                let parsed: ResponseBody =
                    serde_json::from_slice(&bytes).map_err(|_| CheckError::InvalidResponse)?;
                return validate_answers(parsed);
            }
            if (status == 429 || status == 529) && attempt < self.backoff.len() {
                tokio::time::sleep(self.backoff[attempt]).await;
                attempt += 1;
                continue;
            }
            return Err(CheckError::Status(status));
        }
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// The schema version this build writes and expects. Bumping it makes an
/// older database's rows unreadable to this build; [`Store::open`] then
/// leaves the file untouched and reports [`StoreError::UnknownVersion`].
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS skill_checks (
    id INTEGER PRIMARY KEY,
    content_sha256 TEXT NOT NULL,
    questions_sha256 TEXT NOT NULL,
    skill TEXT NOT NULL,
    path TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('rules','typesafe')),
    model TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    result TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS skill_checks_key
ON skill_checks(content_sha256, questions_sha256, id);
"#;

/// A storage failure carries no SQLite text, paths, or row values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreError {
    Io,
    /// `PRAGMA user_version` did not match [`SCHEMA_VERSION`]: an older or
    /// newer build's database. The feature disables itself for the process
    /// rather than reading or writing rows it cannot interpret.
    UnknownVersion,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => formatter.write_str("skill check store unavailable"),
            Self::UnknownVersion => {
                formatter.write_str("skill check database has an unrecognized schema version")
            }
        }
    }
}

impl std::error::Error for StoreError {}

type StoreResult<T> = std::result::Result<T, StoreError>;

/// The four TypeSafe answers, or the fired rule ids. This is exactly what
/// the `result` column stores as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum ResultJson {
    Rules { rules: Vec<String> },
    Answers { answers: AnswerMap },
}

/// One stored row, as read back for display.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    source: String,
    model: String,
    checked_at: String,
    result: ResultJson,
}

struct NewRow<'a> {
    content_sha256: &'a str,
    questions_sha256: &'a str,
    skill: &'a str,
    path: &'a str,
    source: &'static str,
    model: &'a str,
    checked_at: &'a str,
    result: &'a ResultJson,
}

/// The append-only skill-check database, `~/.otto/skill-checks.db`.
#[derive(Debug)]
struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    fn open(filename: &Path) -> StoreResult<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| StoreError::Io)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| StoreError::Io)?;
        }
        let connection = Connection::open_with_flags(
            filename,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| StoreError::Io)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| StoreError::Io)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| StoreError::Io)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|_| StoreError::Io)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    fn open_in_memory() -> StoreResult<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| StoreError::Io)?)
    }

    fn initialize(connection: Connection) -> StoreResult<Self> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| StoreError::Io)?;
        if version == 0 {
            connection
                .execute_batch(SCHEMA)
                .map_err(|_| StoreError::Io)?;
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|_| StoreError::Io)?;
        } else if version != SCHEMA_VERSION {
            return Err(StoreError::UnknownVersion);
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn append(&self, row: &NewRow<'_>) -> StoreResult<()> {
        let result = serde_json::to_string(row.result).map_err(|_| StoreError::Io)?;
        self.connection
            .lock()
            .map_err(|_| StoreError::Io)?
            .execute(
                "INSERT INTO skill_checks (
                    content_sha256, questions_sha256, skill, path, source, model, checked_at, result
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.content_sha256,
                    row.questions_sha256,
                    row.skill,
                    row.path,
                    row.source,
                    row.model,
                    row.checked_at,
                    result,
                ],
            )
            .map_err(|_| StoreError::Io)?;
        Ok(())
    }

    /// The row with the largest id for this key, if any.
    fn latest(&self, content_sha256: &str, questions_sha256: &str) -> StoreResult<Option<Row>> {
        let connection = self.connection.lock().map_err(|_| StoreError::Io)?;
        let mut statement = connection
            .prepare(
                "SELECT source, model, checked_at, result FROM skill_checks
                 WHERE content_sha256 = ?1 AND questions_sha256 = ?2
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|_| StoreError::Io)?;
        let mut rows = statement
            .query(params![content_sha256, questions_sha256])
            .map_err(|_| StoreError::Io)?;
        let Some(row) = rows.next().map_err(|_| StoreError::Io)? else {
            return Ok(None);
        };
        let source: String = row.get(0).map_err(|_| StoreError::Io)?;
        let model: String = row.get(1).map_err(|_| StoreError::Io)?;
        let checked_at: String = row.get(2).map_err(|_| StoreError::Io)?;
        let result_text: String = row.get(3).map_err(|_| StoreError::Io)?;
        let result: ResultJson = serde_json::from_str(&result_text).map_err(|_| StoreError::Io)?;
        Ok(Some(Row {
            source,
            model,
            checked_at,
            result,
        }))
    }
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// A Choice or Score answer's confidence, or a Score/Choice threshold miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hint {
    Ok,
    OkUncertain,
    Flagged,
    FlaggedUncertain,
    NotAsked,
}

impl fmt::Display for Hint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ok => "ok",
            Self::OkUncertain => "ok (uncertain)",
            Self::Flagged => "flag",
            Self::FlaggedUncertain => "flag (uncertain)",
            Self::NotAsked => "not asked",
        })
    }
}

const LOW_CONFIDENCE: f64 = 0.6;
const LOW_NOUL: f64 = 0.5;
const LOW_SCORE: f64 = 1.5;

fn is_flagged(question_id: &str, answer: &serde_json::Value) -> bool {
    match question_id {
        "procedural" => {
            answer.get("choice").and_then(serde_json::Value::as_str) != Some("procedural")
        }
        "self_contained" | "output_usable" => answer
            .get("noul")
            .and_then(serde_json::Value::as_f64)
            .is_none_or(|value| value < LOW_NOUL),
        "body_matches_contract" => answer
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .is_none_or(|value| value < LOW_SCORE),
        _ => false,
    }
}

/// The verdict `/skills` shows for `question_id`, computed from `result` at
/// display time so a threshold change is visible without a new request.
fn hint_for(question_id: &str, result: &ResultJson) -> Hint {
    match result {
        ResultJson::Rules { rules } => {
            let fired = RULES
                .iter()
                .find(|rule| rule.question == question_id && rules.iter().any(|id| id == rule.id));
            if fired.is_some() {
                Hint::Flagged
            } else {
                Hint::NotAsked
            }
        }
        ResultJson::Answers { answers } => match answers.get(question_id) {
            None => Hint::NotAsked,
            Some(answer) => {
                let flagged = is_flagged(question_id, answer);
                // Noul answers carry no confidence field at all; only
                // choice and score answers can be uncertain.
                let uncertain = answer
                    .get("confidence")
                    .and_then(serde_json::Value::as_f64)
                    .is_some_and(|c| c < LOW_CONFIDENCE);
                match (flagged, uncertain) {
                    (true, true) => Hint::FlaggedUncertain,
                    (true, false) => Hint::Flagged,
                    (false, true) => Hint::OkUncertain,
                    (false, false) => Hint::Ok,
                }
            }
        },
    }
}

fn render_row(row: &Row) -> String {
    let questions = QUESTIONS
        .iter()
        .map(|question| format!("{}={}", question.id, hint_for(question.id, &row.result)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {}@{}: {questions}",
        row.source,
        if row.model.is_empty() {
            String::new()
        } else {
            format!("{} ", row.model)
        },
        row.checked_at
    )
}

// ---------------------------------------------------------------------------
// Checker: orchestration
// ---------------------------------------------------------------------------

/// Reads and hashes what a skill directory's `SKILL.md` currently contains.
fn read_skill_md(skill_directory: &Path) -> Option<Vec<u8>> {
    let root_fs = Root::open(skill_directory).ok()?;
    crate::skill::read_root_file(&root_fs, Path::new("SKILL.md")).ok()
}

/// Runs deterministic rules, and if none fires, calls TypeSafe; owns the
/// append-only store, the in-process in-flight set, and the last-failure
/// memory `/skills` reads. Built only when the three enabling conditions in
/// the spec hold; `AgentTool::execute` triggers a check only for
/// skill-derived definitions.
pub struct Checker {
    store: Store,
    client: TypeSafeClient,
    in_flight: Mutex<HashSet<String>>,
    failures: Mutex<HashMap<String, String>>,
}

impl Checker {
    /// Opens `db_path` (creating it, `~/.otto/skill-checks.db` in
    /// production) and builds a client for `base_url`/`api_key`. Returns
    /// [`StoreError`]'s text on an unrecognized schema version or an
    /// unwritable path; the caller prints that once at startup and runs
    /// without a checker.
    pub fn open(db_path: &Path, base_url: &str, api_key: &str) -> Result<Self, String> {
        Ok(Self {
            store: Store::open(db_path).map_err(|error| error.to_string())?,
            client: TypeSafeClient::new(base_url, api_key),
            in_flight: Mutex::new(HashSet::new()),
            failures: Mutex::new(HashMap::new()),
        })
    }

    /// An in-memory checker for tests: no filesystem database, and a
    /// zero-delay backoff so a stub 429 does not sleep.
    #[cfg(test)]
    pub(crate) fn open_in_memory(base_url: &str, api_key: &str) -> Self {
        Self {
            store: Store::open_in_memory().expect("in-memory store"),
            client: TypeSafeClient::new(base_url, api_key).with_backoff(vec![
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            ]),
            in_flight: Mutex::new(HashSet::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Starts a background check of `skill_directory`'s current `SKILL.md`
    /// for `skill_name`/`skill_path`, unless one is already in flight in
    /// this process or the database already has a row for its key. Never
    /// blocks the caller.
    pub fn trigger(
        self: &Arc<Self>,
        skill_name: String,
        skill_directory: PathBuf,
        skill_path: PathBuf,
    ) {
        let checker = Arc::clone(self);
        tokio::spawn(async move {
            checker.run(skill_name, skill_directory, skill_path).await;
        });
    }

    async fn run(&self, skill_name: String, skill_directory: PathBuf, skill_path: PathBuf) {
        let Some(bytes) = read_skill_md(&skill_directory) else {
            return;
        };
        let content_hash = content_sha256(&bytes);
        let question_hash = questions_sha256();
        let key = format!("{content_hash}:{question_hash}");
        {
            let mut in_flight = self.in_flight.lock().expect("in-flight lock");
            if !in_flight.insert(key.clone()) {
                return;
            }
        }
        let outcome = self
            .run_uncached(
                &skill_name,
                &skill_path,
                &bytes,
                &content_hash,
                &question_hash,
            )
            .await;
        self.in_flight.lock().expect("in-flight lock").remove(&key);
        match outcome {
            Ok(()) => {
                self.failures
                    .lock()
                    .expect("failures lock")
                    .remove(&skill_name);
            }
            Err(Some(reason)) => {
                self.failures
                    .lock()
                    .expect("failures lock")
                    .insert(skill_name, reason);
            }
            // Already checked, or unreadable: no failure to remember.
            Err(None) => {}
        }
    }

    /// `Ok(())` on a written row, `Err(Some(reason))` on a failed TypeSafe
    /// call, `Err(None)` when there was nothing to do (already checked, or
    /// the frontmatter did not parse).
    async fn run_uncached(
        &self,
        skill_name: &str,
        skill_path: &Path,
        bytes: &[u8],
        content_hash: &str,
        question_hash: &str,
    ) -> Result<(), Option<String>> {
        if matches!(self.store.latest(content_hash, question_hash), Ok(Some(_))) {
            return Err(None);
        }
        let Ok((fields, body)) = frontmatter::parse(bytes) else {
            return Err(None);
        };
        let output = fields
            .get("output")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let path_text = skill_path.to_string_lossy();
        let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);

        let fired = fired_rules(&body, &output);
        if !fired.is_empty() {
            let result = ResultJson::Rules {
                rules: fired.iter().map(|rule| rule.id.to_string()).collect(),
            };
            let row = NewRow {
                content_sha256: content_hash,
                questions_sha256: question_hash,
                skill: skill_name,
                path: &path_text,
                source: "rules",
                model: "",
                checked_at: &checked_at,
                result: &result,
            };
            self.store.append(&row).map_err(|_| None)?;
            return Ok(());
        }

        let state = String::from_utf8_lossy(bytes).into_owned();
        match self.client.check(&state).await {
            Ok(answers) => {
                let result = ResultJson::Answers {
                    answers: answers.answers,
                };
                let row = NewRow {
                    content_sha256: content_hash,
                    questions_sha256: question_hash,
                    skill: skill_name,
                    path: &path_text,
                    source: "typesafe",
                    model: &answers.model,
                    checked_at: &checked_at,
                    result: &result,
                };
                self.store.append(&row).map_err(|_| None)?;
                Ok(())
            }
            Err(error) => Err(Some(error.reason())),
        }
    }

    /// The line `/skills` shows for `skill`: the latest result for its
    /// current `SKILL.md`, `not checked yet`, or `not checked yet (last
    /// attempt: <reason>)`.
    pub fn display(&self, skill: &crate::skill::Skill) -> String {
        let Some(bytes) = read_skill_md(&skill.directory) else {
            return "not checked yet".to_string();
        };
        let content_hash = content_sha256(&bytes);
        let question_hash = questions_sha256();
        if let Ok(Some(row)) = self.store.latest(&content_hash, &question_hash) {
            return render_row(&row);
        }
        match self
            .failures
            .lock()
            .expect("failures lock")
            .get(&skill.name)
        {
            Some(reason) => format!("not checked yet (last attempt: {reason})"),
            None => "not checked yet".to_string(),
        }
    }
}

#[cfg(test)]
mod tests;
