//! Durable, workspace-scoped multi-agent workflows.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::subagent::WritePolicy;

const MAX_DEFINITION_BYTES: usize = 1 << 20;
const MAX_TEXT_BYTES: usize = 64 << 10;
const MAX_DESCRIPTION_CHARS: usize = 1024;
const MAX_STEPS: usize = 32;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS workflow_runs (
    id TEXT PRIMARY KEY,
    workflow TEXT NOT NULL,
    definition_json TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    workspace TEXT NOT NULL,
    profile TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'running','waiting','paused','succeeded','failed','canceled'
    )),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_steps (
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    kind TEXT NOT NULL CHECK (kind IN ('agent','approval','handoff')),
    agent TEXT NOT NULL,
    prompt TEXT NOT NULL,
    needs_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'pending','ready','running','waiting','succeeded','failed','canceled','interrupted'
    )),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    transcript_path TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_attempts (
    run_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    status TEXT NOT NULL CHECK (status IN (
        'running','succeeded','failed','canceled','interrupted'
    )),
    transcript_path TEXT NOT NULL,
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    started_at TEXT NOT NULL,
    finished_at TEXT,
    PRIMARY KEY (run_id, step_id, attempt),
    FOREIGN KEY (run_id, step_id) REFERENCES workflow_steps(run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_requests (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','approved','rejected','canceled')),
    created_at TEXT NOT NULL,
    responded_at TEXT,
    UNIQUE (run_id, step_id),
    FOREIGN KEY (run_id, step_id) REFERENCES workflow_steps(run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_events (
    seq INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    step_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT ''
) STRICT;
CREATE INDEX IF NOT EXISTS workflow_events_run ON workflow_events(run_id, seq);
"#;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub description: String,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub agents: Vec<AgentSnapshot>,
    pub hash: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: String,
    pub context: String,
    #[serde(default)]
    pub write_policy: SnapshotWritePolicy,
    #[serde(default)]
    pub write_paths: Vec<String>,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotWritePolicy {
    ReadOnly,
    ProposeOnly,
    #[default]
    SingleWriter,
    OwnedPaths,
}

impl SnapshotWritePolicy {
    fn from_subagent(policy: WritePolicy) -> Self {
        match policy {
            WritePolicy::ReadOnly => Self::ReadOnly,
            WritePolicy::ProposeOnly => Self::ProposeOnly,
            WritePolicy::SingleWriter => Self::SingleWriter,
            WritePolicy::OwnedPaths => Self::OwnedPaths,
        }
    }

    pub fn as_subagent(self) -> WritePolicy {
        match self {
            Self::ReadOnly => WritePolicy::ReadOnly,
            Self::ProposeOnly => WritePolicy::ProposeOnly,
            Self::SingleWriter => WritePolicy::SingleWriter,
            Self::OwnedPaths => WritePolicy::OwnedPaths,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    definitions: Vec<Definition>,
}

impl Catalog {
    #[cfg(test)]
    pub(crate) fn from_definitions(definitions: Vec<Definition>) -> Self {
        Self { definitions }
    }

    pub fn discover(
        roots: &[std::path::PathBuf],
        agents: &crate::subagent::Catalog,
    ) -> (Self, Vec<String>) {
        let mut by_name = std::collections::BTreeMap::new();
        let mut warnings = Vec::new();
        for root in roots {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!("workflows root {}: {error}", root.display()));
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                    continue;
                }
                match entry.file_type() {
                    Ok(file_type) if file_type.is_file() => {}
                    _ => continue,
                }
                let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let data = match read_definition_file(&path) {
                    Ok(data) => data,
                    Err(error) => {
                        warnings.push(format!("workflow {}: {error}", path.display()));
                        continue;
                    }
                };
                match parse_definition(name, &data, &|agent| agents.lookup(agent).is_some()) {
                    Ok(mut definition) => {
                        let names: HashSet<String> = definition
                            .steps
                            .iter()
                            .filter(|step| matches!(step.kind, StepKind::Agent | StepKind::Handoff))
                            .map(|step| step.agent.clone())
                            .collect();
                        definition.agents = names
                            .into_iter()
                            .filter_map(|name| agents.lookup(&name))
                            .map(|agent| AgentSnapshot {
                                name: agent.name.clone(),
                                description: agent.description.clone(),
                                tools: agent.tools.clone(),
                                model: agent.model.clone(),
                                context: agent.context.clone(),
                                write_policy: SnapshotWritePolicy::from_subagent(
                                    agent.write_policy,
                                ),
                                write_paths: agent.write_paths.clone(),
                                body: agent.body.clone(),
                            })
                            .collect();
                        definition
                            .agents
                            .sort_by(|left, right| left.name.cmp(&right.name));
                        if let Err(error) = validate_write_coordination(&definition) {
                            warnings.push(format!("workflow {}: {error}", path.display()));
                            continue;
                        }
                        definition.hash.clear();
                        definition.hash = format!(
                            "{:x}",
                            Sha256::digest(serde_json::to_vec(&definition).unwrap_or_default())
                        );
                        by_name.insert(definition.name.clone(), definition);
                    }
                    Err(error) => warnings.push(format!("workflow {}: {error}", path.display())),
                }
            }
        }
        (
            Self {
                definitions: by_name.into_values().collect(),
            },
            warnings,
        )
    }

    pub fn lookup(&self, name: &str) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.name == name)
    }

    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }
}

pub(crate) fn validate_write_coordination(definition: &Definition) -> Result<(), String> {
    let agents: HashMap<&str, &AgentSnapshot> = definition
        .agents
        .iter()
        .map(|agent| (agent.name.as_str(), agent))
        .collect();
    for (left_index, left) in definition.steps.iter().enumerate() {
        if left.kind != StepKind::Agent {
            continue;
        }
        let Some(left_agent) = agents.get(left.agent.as_str()) else {
            continue;
        };
        let Some(left_writer) = writer_scope(left_agent) else {
            continue;
        };
        for right in definition.steps.iter().skip(left_index + 1) {
            if right.kind != StepKind::Agent || ordered_by_dependency(definition, left, right) {
                continue;
            }
            let Some(right_agent) = agents.get(right.agent.as_str()) else {
                continue;
            };
            let Some(right_writer) = writer_scope(right_agent) else {
                continue;
            };
            if writer_scopes_conflict(&left_writer, &right_writer) {
                return Err(format!(
                    "steps {:?} and {:?} may run concurrently and both can write; add a dependency or use disjoint owned_paths write policies",
                    left.id, right.id
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum WriterScope<'a> {
    Global,
    Owned(&'a [String]),
}

fn writer_scope(agent: &AgentSnapshot) -> Option<WriterScope<'_>> {
    if !agent_can_mutate(agent) {
        return None;
    }
    match agent.write_policy {
        SnapshotWritePolicy::ReadOnly | SnapshotWritePolicy::ProposeOnly => None,
        SnapshotWritePolicy::SingleWriter => Some(WriterScope::Global),
        SnapshotWritePolicy::OwnedPaths => Some(WriterScope::Owned(&agent.write_paths)),
    }
}

fn agent_can_mutate(agent: &AgentSnapshot) -> bool {
    agent.tools.as_ref().is_none_or(|tools| {
        tools
            .iter()
            .any(|tool| matches!(tool.as_str(), "write" | "edit"))
    })
}

fn writer_scopes_conflict(left: &WriterScope<'_>, right: &WriterScope<'_>) -> bool {
    match (left, right) {
        (WriterScope::Global, _) | (_, WriterScope::Global) => true,
        (WriterScope::Owned(left), WriterScope::Owned(right)) => left.iter().any(|left| {
            right
                .iter()
                .any(|right| ownership_patterns_overlap(left, right))
        }),
    }
}

fn ownership_patterns_overlap(left: &str, right: &str) -> bool {
    left == right
        || pattern_prefix(left).is_some_and(|prefix| right.starts_with(prefix))
        || pattern_prefix(right).is_some_and(|prefix| left.starts_with(prefix))
}

fn pattern_prefix(pattern: &str) -> Option<&str> {
    pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("/*"))
        .or_else(|| pattern.split_once('*').map(|(prefix, _)| prefix))
}

fn ordered_by_dependency(definition: &Definition, left: &Step, right: &Step) -> bool {
    depends_on(definition, &left.id, &right.id) || depends_on(definition, &right.id, &left.id)
}

fn depends_on(definition: &Definition, step_id: &str, dependency_id: &str) -> bool {
    let by_id: HashMap<&str, &Step> = definition
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect();
    let mut stack = vec![step_id];
    let mut seen = HashSet::new();
    while let Some(current) = stack.pop() {
        let Some(step) = by_id.get(current) else {
            continue;
        };
        for dependency in &step.needs {
            if dependency == dependency_id {
                return true;
            }
            if seen.insert(dependency.as_str()) {
                stack.push(dependency);
            }
        }
    }
    false
}

fn read_definition_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if file.metadata()?.len() > MAX_DEFINITION_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds 1 MiB",
        ));
    }
    let mut data = Vec::new();
    file.by_ref()
        .take((MAX_DEFINITION_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    if data.len() > MAX_DEFINITION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds 1 MiB",
        ));
    }
    Ok(data)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    pub agent: String,
    pub prompt: String,
    pub needs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    #[default]
    Agent,
    Handoff,
    Approval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Waiting,
    Paused,
    Succeeded,
    Failed,
    Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Ready,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Canceled,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: String,
    pub workflow: String,
    pub workspace: String,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub input: String,
    pub status: RunStatus,
    pub steps: Vec<StepRecord>,
    #[serde(skip_serializing)]
    pub definition: Definition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepRecord {
    pub id: String,
    pub kind: StepKind,
    pub agent: String,
    pub prompt: String,
    pub needs: Vec<String>,
    pub status: StepStatus,
    pub attempt: u32,
    pub result: String,
    pub error: String,
    pub transcript_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub run_id: String,
    pub step_id: String,
    pub prompt: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EventRecord {
    pub seq: i64,
    pub run_id: String,
    pub occurred_at: String,
    pub kind: String,
    pub step_id: String,
    pub status: String,
}

pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| unavailable())?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| unavailable())?;
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| unavailable())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| unavailable())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| unavailable())?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|_| unavailable())?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Self {
        Self::initialize(Connection::open_in_memory().expect("sqlite")).expect("schema")
    }

    fn initialize(connection: Connection) -> Result<Self, String> {
        connection
            .execute_batch(SCHEMA)
            .map_err(|_| unavailable())?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_run(
        &self,
        id: &str,
        definition: &Definition,
        workspace: &str,
        profile: &str,
        provider: &str,
        model: &str,
        input: &str,
    ) -> Result<Run, String> {
        if input.len() > MAX_TEXT_BYTES {
            return Err("workflow input exceeds 65536 UTF-8 bytes".to_string());
        }
        let definition_json = serde_json::to_string(definition).map_err(|_| unavailable())?;
        let now = timestamp();
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        transaction
            .execute(
                "INSERT INTO workflow_runs (
                    id, workflow, definition_json, definition_hash, workspace,
                    profile, provider, model, input, status, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'running', ?10, ?10)",
                params![
                    id,
                    definition.name,
                    definition_json,
                    definition.hash,
                    workspace,
                    profile,
                    provider,
                    model,
                    input,
                    now,
                ],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, id, "run_started", "", "running")?;
        for (position, step) in definition.steps.iter().enumerate() {
            let status = if step.needs.is_empty() {
                StepStatus::Ready
            } else {
                StepStatus::Pending
            };
            transaction
                .execute(
                    "INSERT INTO workflow_steps (
                        run_id, id, position, kind, agent, prompt, needs_json, status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        id,
                        step.id,
                        position as i64,
                        step.kind.as_str(),
                        step.agent,
                        step.prompt,
                        serde_json::to_string(&step.needs).map_err(|_| unavailable())?,
                        status.as_str(),
                    ],
                )
                .map_err(|_| unavailable())?;
            if status == StepStatus::Ready {
                event(&transaction, id, "step_ready", &step.id, status.as_str())?;
            }
        }
        transaction.commit().map_err(|_| unavailable())?;
        drop(connection);
        self.get_run(id)?.ok_or_else(unavailable)
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>, String> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let header = connection
            .query_row(
                "SELECT workflow, workspace, profile, provider, model, input, status,
                        definition_json
                 FROM workflow_runs WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some((workflow, workspace, profile, provider, model, input, status, definition_json)) =
            header
        else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare(
                "SELECT id, kind, agent, prompt, needs_json, status, attempt,
                        result, error, transcript_path
                 FROM workflow_steps WHERE run_id = ?1 ORDER BY position",
            )
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                ))
            })
            .map_err(|_| unavailable())?;
        let mut steps = Vec::new();
        for row in rows {
            let (step_id, kind, agent, prompt, needs, status, attempt, result, error, path) =
                row.map_err(|_| unavailable())?;
            steps.push(StepRecord {
                id: step_id,
                kind: StepKind::parse(&kind)?,
                agent,
                prompt,
                needs: serde_json::from_str(&needs).map_err(|_| unavailable())?,
                status: StepStatus::parse(&status)?,
                attempt: u32::try_from(attempt).map_err(|_| unavailable())?,
                result,
                error,
                transcript_path: path,
            });
        }
        Ok(Some(Run {
            id: id.to_string(),
            workflow,
            workspace,
            profile,
            provider,
            model,
            input,
            status: RunStatus::parse(&status)?,
            steps,
            definition: serde_json::from_str(&definition_json).map_err(|_| unavailable())?,
        }))
    }

    pub fn claim_ready(
        &self,
        run_id: &str,
        step_id: &str,
        transcript_path: &str,
    ) -> Result<u32, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let current = transaction
            .query_row(
                "SELECT status, attempt FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(|| "workflow step not found".to_string())?;
        if current.0 != "ready" {
            return Err("workflow step is not ready".to_string());
        }
        let attempt = current.1.checked_add(1).ok_or_else(unavailable)?;
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'running', attempt = ?3,
                    transcript_path = ?4, result = '', error = ''
                 WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id, attempt, transcript_path],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "INSERT INTO workflow_attempts (
                    run_id, step_id, attempt, status, transcript_path, started_at
                 ) VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
                params![run_id, step_id, attempt, transcript_path, now],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "step_started", step_id, "running")?;
        transaction.commit().map_err(|_| unavailable())?;
        u32::try_from(attempt).map_err(|_| unavailable())
    }

    pub fn finish_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        result: Result<&str, &str>,
    ) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let matches: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM workflow_steps
                 WHERE run_id = ?1 AND id = ?2 AND status = 'running' AND attempt = ?3",
                params![run_id, step_id, attempt],
                |row| row.get(0),
            )
            .map_err(|_| unavailable())?;
        if matches != 1 {
            return Err("workflow attempt is not running".to_string());
        }
        let now = timestamp();
        match result {
            Ok(result) => {
                transaction
                    .execute(
                        "UPDATE workflow_attempts SET status = 'succeeded', result = ?4,
                            finished_at = ?5
                         WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                        params![run_id, step_id, attempt, result, now],
                    )
                    .map_err(|_| unavailable())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'succeeded', result = ?3
                         WHERE run_id = ?1 AND id = ?2",
                        params![run_id, step_id, result],
                    )
                    .map_err(|_| unavailable())?;
                event(&transaction, run_id, "step_succeeded", step_id, "succeeded")?;
                advance_ready(&transaction, run_id)?;
                settle_run(&transaction, run_id)?;
            }
            Err(error) => {
                transaction
                    .execute(
                        "UPDATE workflow_attempts SET status = 'failed', error = ?4,
                            finished_at = ?5
                         WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                        params![run_id, step_id, attempt, error, now],
                    )
                    .map_err(|_| unavailable())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'failed', error = ?3
                         WHERE run_id = ?1 AND id = ?2",
                        params![run_id, step_id, error],
                    )
                    .map_err(|_| unavailable())?;
                transaction
                    .execute(
                        "UPDATE workflow_steps SET status = 'canceled'
                         WHERE run_id = ?1 AND status IN ('pending','ready','waiting')",
                        [run_id],
                    )
                    .map_err(|_| unavailable())?;
                event(&transaction, run_id, "step_failed", step_id, "failed")?;
            }
        }
        transaction.commit().map_err(|_| unavailable())
    }

    pub fn recover(&self, workspace: &str) -> Result<usize, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let running = {
            let mut statement = transaction
                .prepare(
                    "SELECT s.run_id, s.id, s.attempt
                     FROM workflow_steps s
                     JOIN workflow_runs r ON r.id = s.run_id
                     WHERE s.status = 'running' AND r.workspace = ?1",
                )
                .map_err(|_| unavailable())?;
            let rows = statement
                .query_map([workspace], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|_| unavailable())?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|_| unavailable())?
        };
        let mut runs = HashSet::new();
        for (run_id, step_id, attempt) in &running {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'interrupted'
                     WHERE run_id = ?1 AND id = ?2",
                    params![run_id, step_id],
                )
                .map_err(|_| unavailable())?;
            transaction
                .execute(
                    "UPDATE workflow_attempts SET status = 'interrupted', finished_at = ?4
                     WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3",
                    params![run_id, step_id, attempt, timestamp()],
                )
                .map_err(|_| unavailable())?;
            event(
                &transaction,
                run_id,
                "step_interrupted",
                step_id,
                "interrupted",
            )?;
            runs.insert(run_id.clone());
        }
        for run_id in &runs {
            transaction
                .execute(
                    "UPDATE workflow_runs SET status = 'paused', updated_at = ?2 WHERE id = ?1",
                    params![run_id, timestamp()],
                )
                .map_err(|_| unavailable())?;
            event(&transaction, run_id, "run_paused", "", "paused")?;
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(running.len())
    }

    pub fn pause_run(&self, run_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let status: String = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .map_err(|_| unavailable())?;
        if !matches!(status.as_str(), "running" | "waiting") {
            return Ok(());
        }
        let attempts = {
            let mut statement = transaction
                .prepare(
                    "SELECT id, attempt FROM workflow_steps
                     WHERE run_id = ?1 AND attempt > 0 AND status IN ('running','canceled')",
                )
                .map_err(|_| unavailable())?;
            let rows = statement
                .query_map([run_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|_| unavailable())?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|_| unavailable())?
        };
        for (step_id, attempt) in attempts {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'interrupted'
                     WHERE run_id = ?1 AND id = ?2 AND status IN ('running','canceled')",
                    params![run_id, step_id],
                )
                .map_err(|_| unavailable())?;
            transaction
                .execute(
                    "UPDATE workflow_attempts SET status = 'interrupted', finished_at = ?4
                     WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3
                       AND status IN ('running','canceled')",
                    params![run_id, step_id, attempt, timestamp()],
                )
                .map_err(|_| unavailable())?;
            event(
                &transaction,
                run_id,
                "step_interrupted",
                &step_id,
                "interrupted",
            )?;
        }
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'paused', updated_at = ?2
                 WHERE id = ?1 AND status IN ('running','waiting')",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "run_paused", "", "paused")?;
        transaction.commit().map_err(|_| unavailable())
    }

    pub fn finalize_failed(&self, run_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let (running, failed): (i64, i64) = transaction
            .query_row(
                "SELECT
                    SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END)
                 FROM workflow_steps WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| unavailable())?;
        if running != 0 || failed == 0 {
            return Err("workflow failure is not ready to finalize".to_string());
        }
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'failed', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "run_failed", "", "failed")?;
        transaction.commit().map_err(|_| unavailable())
    }

    pub fn request_approval(
        &self,
        request_id: &str,
        run_id: &str,
        step_id: &str,
    ) -> Result<ApprovalRequest, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let row = transaction
            .query_row(
                "SELECT kind, status, prompt FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(|| "workflow step not found".to_string())?;
        if row.0 != "approval" || row.1 != "ready" {
            return Err("workflow approval step is not ready".to_string());
        }
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'waiting' WHERE run_id = ?1 AND id = ?2",
                params![run_id, step_id],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "INSERT INTO workflow_requests (
                    id, run_id, step_id, prompt, status, created_at
                 ) VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
                params![request_id, run_id, step_id, row.2, now],
            )
            .map_err(|_| unavailable())?;
        event(
            &transaction,
            run_id,
            "approval_requested",
            step_id,
            "waiting",
        )?;
        settle_run(&transaction, run_id)?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(ApprovalRequest {
            id: request_id.to_string(),
            run_id: run_id.to_string(),
            step_id: step_id.to_string(),
            prompt: row.2,
            status: "pending".to_string(),
        })
    }

    pub fn respond(&self, request_id: &str, approved: bool) -> Result<String, String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let row = transaction
            .query_row(
                "SELECT run_id, step_id, status FROM workflow_requests WHERE id = ?1",
                [request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(|| "approval request not found".to_string())?;
        let wanted = if approved { "approved" } else { "rejected" };
        if row.2 == wanted {
            return Ok(row.0);
        }
        if row.2 != "pending" {
            return Err("approval request already answered differently".to_string());
        }
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_requests SET status = ?2, responded_at = ?3 WHERE id = ?1",
                params![request_id, wanted, now],
            )
            .map_err(|_| unavailable())?;
        if approved {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'succeeded', result = 'approved'
                     WHERE run_id = ?1 AND id = ?2 AND status = 'waiting'",
                    params![row.0, row.1],
                )
                .map_err(|_| unavailable())?;
            event(
                &transaction,
                &row.0,
                "approval_approved",
                &row.1,
                "succeeded",
            )?;
            advance_ready(&transaction, &row.0)?;
            settle_run(&transaction, &row.0)?;
        } else {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'canceled'
                     WHERE run_id = ?1 AND status NOT IN ('succeeded','failed','canceled')",
                    [&row.0],
                )
                .map_err(|_| unavailable())?;
            transaction
                .execute(
                    "UPDATE workflow_runs SET status = 'canceled', updated_at = ?2 WHERE id = ?1",
                    params![row.0, now],
                )
                .map_err(|_| unavailable())?;
            event(
                &transaction,
                &row.0,
                "approval_rejected",
                &row.1,
                "canceled",
            )?;
            event(&transaction, &row.0, "run_canceled", "", "canceled")?;
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(row.0)
    }

    pub fn retry(&self, run_id: &str, step_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let changed = transaction
            .execute(
                "UPDATE workflow_steps SET status = 'ready', result = '', error = ''
                 WHERE run_id = ?1 AND id = ?2 AND status = 'interrupted'",
                params![run_id, step_id],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err("workflow step is not interrupted".to_string());
        }
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'running', updated_at = ?2
                 WHERE id = ?1 AND status = 'paused'",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "step_ready", step_id, "ready")?;
        event(&transaction, run_id, "run_resumed", "", "running")?;
        transaction.commit().map_err(|_| unavailable())
    }

    pub fn cancel_attempt(&self, run_id: &str, step_id: &str, attempt: u32) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let now = timestamp();
        transaction
            .execute(
                "UPDATE workflow_attempts SET status = 'canceled', finished_at = ?4
                 WHERE run_id = ?1 AND step_id = ?2 AND attempt = ?3 AND status = 'running'",
                params![run_id, step_id, attempt, now],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'canceled'
                 WHERE run_id = ?1 AND id = ?2 AND status = 'running' AND attempt = ?3",
                params![run_id, step_id, attempt],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "step_canceled", step_id, "canceled")?;
        transaction.commit().map_err(|_| unavailable())
    }

    pub fn cancel_run(&self, run_id: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection.transaction().map_err(|_| unavailable())?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some(status) = status else {
            return Err("workflow run not found".to_string());
        };
        if matches!(status.as_str(), "succeeded" | "failed" | "canceled") {
            return Err("workflow run already finished".to_string());
        }
        transaction
            .execute(
                "UPDATE workflow_steps SET status = 'canceled'
                 WHERE run_id = ?1 AND status NOT IN ('succeeded','failed','canceled')",
                [run_id],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "UPDATE workflow_requests SET status = 'canceled', responded_at = ?2
                 WHERE run_id = ?1 AND status = 'pending'",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'canceled', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(&transaction, run_id, "run_canceled", "", "canceled")?;
        transaction.commit().map_err(|_| unavailable())
    }

    pub fn list_runs(&self, workspace: &str) -> Result<Vec<Run>, String> {
        let ids = {
            let connection = self.connection.lock().map_err(|_| unavailable())?;
            let mut statement = connection
                .prepare(
                    "SELECT id FROM workflow_runs WHERE workspace = ?1 ORDER BY created_at DESC",
                )
                .map_err(|_| unavailable())?;
            let rows = statement
                .query_map([workspace], |row| row.get::<_, String>(0))
                .map_err(|_| unavailable())?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|_| unavailable())?
        };
        ids.into_iter()
            .map(|id| self.get_run(&id)?.ok_or_else(unavailable))
            .collect()
    }

    pub fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, String> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let mut statement = connection
            .prepare(
                "SELECT id, step_id, prompt, status FROM workflow_requests
                 WHERE run_id = ?1 ORDER BY created_at",
            )
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok(ApprovalRequest {
                    id: row.get(0)?,
                    run_id: run_id.to_string(),
                    step_id: row.get(1)?,
                    prompt: row.get(2)?,
                    status: row.get(3)?,
                })
            })
            .map_err(|_| unavailable())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| unavailable())
    }

    pub fn request(&self, request_id: &str) -> Result<Option<ApprovalRequest>, String> {
        self.connection
            .lock()
            .map_err(|_| unavailable())?
            .query_row(
                "SELECT run_id, step_id, prompt, status FROM workflow_requests WHERE id = ?1",
                [request_id],
                |row| {
                    Ok(ApprovalRequest {
                        id: request_id.to_string(),
                        run_id: row.get(0)?,
                        step_id: row.get(1)?,
                        prompt: row.get(2)?,
                        status: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|_| unavailable())
    }

    pub fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, String> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let mut statement = connection
            .prepare(
                "SELECT seq, occurred_at, kind, step_id, status FROM workflow_events
                 WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map(params![run_id, after], |row| {
                Ok(EventRecord {
                    seq: row.get(0)?,
                    run_id: run_id.to_string(),
                    occurred_at: row.get(1)?,
                    kind: row.get(2)?,
                    step_id: row.get(3)?,
                    status: row.get(4)?,
                })
            })
            .map_err(|_| unavailable())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| unavailable())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub profile: String,
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub run_id: String,
    pub step_id: String,
    pub agent: String,
    pub agent_definition: AgentSnapshot,
    pub prompt: String,
    pub attempt: u32,
    pub session_id: String,
    pub transcript_root: String,
    pub transcript_path: String,
}

#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, attempt: Attempt, cancel: &CancellationToken)
    -> Result<String, String>;

    async fn close(&self) {}
}

struct ActiveRun {
    cancel: CancellationToken,
    done: Arc<Notify>,
}

pub struct Controller {
    store: Arc<Store>,
    catalog: Catalog,
    executor: Arc<dyn Executor>,
    workspace: String,
    transcript_root: PathBuf,
    runtime: RuntimeIdentity,
    semaphore: Arc<Semaphore>,
    active: Mutex<HashMap<String, ActiveRun>>,
    guard: Mutex<Option<Box<dyn Send>>>,
}

impl Controller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        catalog: Catalog,
        executor: Arc<dyn Executor>,
        workspace: String,
        transcript_root: PathBuf,
        runtime: RuntimeIdentity,
        max_parallel: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            catalog,
            executor,
            workspace,
            transcript_root,
            runtime,
            semaphore: Arc::new(Semaphore::new(max_parallel.max(1))),
            active: Mutex::new(HashMap::new()),
            guard: Mutex::new(None),
        })
    }

    pub fn set_guard(&self, guard: Box<dyn Send>) {
        *self
            .guard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(guard);
    }

    pub async fn start(self: &Arc<Self>, workflow: &str, input: &str) -> Result<Run, String> {
        let definition = self
            .catalog
            .lookup(workflow)
            .ok_or_else(|| format!("workflow {workflow:?} not found"))?;
        validate_write_coordination(definition)?;
        let id = random_id().map_err(|_| "generate workflow id failed".to_string())?;
        let run = self.store.create_run(
            &id,
            definition,
            &self.workspace,
            &self.runtime.profile,
            &self.runtime.provider,
            &self.runtime.model,
            input,
        )?;
        self.launch(id)?;
        Ok(run)
    }

    pub fn get(&self, run_id: &str) -> Result<Run, String> {
        let run = self
            .store
            .get_run(run_id)?
            .ok_or_else(|| "workflow run not found".to_string())?;
        if run.workspace != self.workspace {
            return Err("workflow run not found".to_string());
        }
        Ok(run)
    }

    pub fn list(&self) -> Result<Vec<Run>, String> {
        self.store.list_runs(&self.workspace)
    }

    pub fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, String> {
        self.get(run_id)?;
        self.store.requests(run_id)
    }

    pub fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, String> {
        self.get(run_id)?;
        self.store.events(run_id, after)
    }

    pub async fn wait(&self, run_id: &str) -> Result<Run, String> {
        let done = self
            .active
            .lock()
            .map_err(|_| "workflow runtime unavailable".to_string())?
            .get(run_id)
            .map(|active| Arc::clone(&active.done));
        if let Some(done) = done {
            done.notified().await;
        }
        self.get(run_id)
    }

    pub async fn resume(
        self: &Arc<Self>,
        run_id: &str,
        retry_step: Option<&str>,
    ) -> Result<Run, String> {
        let run = self.get(run_id)?;
        if run.profile != self.runtime.profile
            || run.provider != self.runtime.provider
            || run.model != self.runtime.model
        {
            return Err("workflow runtime no longer matches the stored run".to_string());
        }
        if let Some(step_id) = retry_step {
            self.store.retry(run_id, step_id)?;
        } else if run.status == RunStatus::Paused {
            return Err("an interrupted step must be selected for retry".to_string());
        }
        let current = self.get(run_id)?;
        if current.status == RunStatus::Running {
            self.launch(run_id.to_string())?;
        }
        Ok(current)
    }

    pub fn respond(self: &Arc<Self>, request_id: &str, approved: bool) -> Result<Run, String> {
        let request = self
            .store
            .request(request_id)?
            .ok_or_else(|| "approval request not found".to_string())?;
        self.get(&request.run_id)?;
        let run_id = self.store.respond(request_id, approved)?;
        let run = self.get(&run_id)?;
        if approved && run.status == RunStatus::Running {
            self.launch(run_id)?;
        }
        Ok(run)
    }

    pub async fn cancel(&self, run_id: &str) -> Result<Run, String> {
        let active = self
            .active
            .lock()
            .map_err(|_| "workflow runtime unavailable".to_string())?
            .get(run_id)
            .map(|active| (active.cancel.clone(), Arc::clone(&active.done)));
        if let Some((cancel, done)) = active {
            cancel.cancel();
            done.notified().await;
        }
        self.store.cancel_run(run_id)?;
        self.get(run_id)
    }

    pub async fn close(&self) {
        let active: Vec<(String, CancellationToken, Arc<Notify>)> = self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .map(|(run_id, active)| {
                (
                    run_id.clone(),
                    active.cancel.clone(),
                    Arc::clone(&active.done),
                )
            })
            .collect();
        for (_, cancel, _) in &active {
            cancel.cancel();
        }
        for (run_id, _, done) in active {
            done.notified().await;
            let _ = self.store.pause_run(&run_id);
        }
        self.executor.close().await;
    }

    fn launch(self: &Arc<Self>, run_id: String) -> Result<(), String> {
        let cancel = CancellationToken::new();
        let done = Arc::new(Notify::new());
        {
            let mut active = self
                .active
                .lock()
                .map_err(|_| "workflow runtime unavailable".to_string())?;
            if active.contains_key(&run_id) {
                return Ok(());
            }
            active.insert(
                run_id.clone(),
                ActiveRun {
                    cancel: cancel.clone(),
                    done: Arc::clone(&done),
                },
            );
        }
        let controller = Arc::clone(self);
        tokio::spawn(async move {
            if controller.drive(&run_id, &cancel).await.is_err() {
                let _ = controller.store.pause_run(&run_id);
            }
            controller
                .active
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&run_id);
            done.notify_one();
        });
        Ok(())
    }

    async fn drive(&self, run_id: &str, cancel: &CancellationToken) -> Result<(), String> {
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let run = self.get(run_id)?;
            if run.status != RunStatus::Running {
                return Ok(());
            }
            let ready: Vec<StepRecord> = run
                .steps
                .iter()
                .filter(|step| step.status == StepStatus::Ready)
                .cloned()
                .collect();
            if ready.is_empty() {
                return Ok(());
            }

            let mut agents = Vec::new();
            for step in ready {
                match step.kind {
                    StepKind::Approval => {
                        let request_id =
                            random_id().map_err(|_| "generate workflow id failed".to_string())?;
                        self.store.request_approval(&request_id, run_id, &step.id)?;
                    }
                    StepKind::Agent | StepKind::Handoff => agents.push(step),
                }
            }
            if agents.is_empty() {
                continue;
            }

            let mut attempts = JoinSet::new();
            for step in agents {
                let permit = Arc::clone(&self.semaphore)
                    .acquire_owned()
                    .await
                    .map_err(|_| "workflow runtime unavailable".to_string())?;
                if cancel.is_cancelled() {
                    drop(permit);
                    return Ok(());
                }
                let next_attempt = step.attempt.saturating_add(1);
                let run_root = self.transcript_root.join(run_id);
                let step_root = run_root.join(&step.id);
                let attempt_root = step_root.join(next_attempt.to_string());
                for directory in [&self.transcript_root, &run_root, &step_root, &attempt_root] {
                    secure_directory(directory)?;
                }
                let session_id =
                    random_id().map_err(|_| "generate workflow session id failed".to_string())?;
                let workspace_key = crate::session::workspace_key(Path::new(&run.workspace))
                    .map_err(|_| "resolve workflow transcript path failed".to_string())?;
                let transcript_path = attempt_root
                    .join(workspace_key)
                    .join(format!("{session_id}.jsonl"))
                    .to_string_lossy()
                    .into_owned();
                let attempt_number = self.store.claim_ready(run_id, &step.id, &transcript_path)?;
                let attempt = Attempt {
                    run_id: run_id.to_string(),
                    step_id: step.id.clone(),
                    agent: step.agent.clone(),
                    agent_definition: run
                        .definition
                        .agents
                        .iter()
                        .find(|definition| definition.name == step.agent)
                        .cloned()
                        .ok_or_else(|| "workflow agent snapshot is missing".to_string())?,
                    prompt: attempt_prompt(&run, &step)?,
                    attempt: attempt_number,
                    session_id,
                    transcript_root: attempt_root.to_string_lossy().into_owned(),
                    transcript_path,
                };
                let executor = Arc::clone(&self.executor);
                let attempt_cancel = cancel.clone();
                attempts.spawn(async move {
                    let result = executor.execute(attempt.clone(), &attempt_cancel).await;
                    drop(permit);
                    (attempt, result, attempt_cancel.is_cancelled())
                });
            }

            let mut failed = false;
            while let Some(joined) = attempts.join_next().await {
                let (attempt, result, canceled) =
                    joined.map_err(|_| "workflow attempt task failed".to_string())?;
                if canceled {
                    self.store.cancel_attempt(
                        &attempt.run_id,
                        &attempt.step_id,
                        attempt.attempt,
                    )?;
                    continue;
                }
                match result {
                    Ok(result) => self.store.finish_attempt(
                        &attempt.run_id,
                        &attempt.step_id,
                        attempt.attempt,
                        Ok(&result),
                    )?,
                    Err(error) => {
                        self.store.finish_attempt(
                            &attempt.run_id,
                            &attempt.step_id,
                            attempt.attempt,
                            Err(&error),
                        )?;
                        cancel.cancel();
                        failed = true;
                    }
                }
            }
            if failed {
                self.store.finalize_failed(run_id)?;
                return Ok(());
            }
        }
    }
}

fn attempt_prompt(run: &Run, step: &StepRecord) -> Result<String, String> {
    let mut prompt = step.prompt.clone();
    if !run.input.is_empty() {
        prompt.push_str("\n\n## Workflow input\n");
        prompt.push_str(&run.input);
    }
    if step.kind == StepKind::Handoff {
        prompt.push_str("\n\n## Handoff\nYou are receiving control from: ");
        prompt.push_str(&step.needs.join(", "));
    }
    for dependency in &step.needs {
        let result = run
            .steps
            .iter()
            .find(|candidate| &candidate.id == dependency)
            .filter(|candidate| candidate.status == StepStatus::Succeeded)
            .map(|candidate| candidate.result.as_str())
            .ok_or_else(|| format!("workflow dependency {dependency:?} has no result"))?;
        prompt.push_str("\n\n## Result from ");
        prompt.push_str(dependency);
        prompt.push('\n');
        prompt.push_str(result);
    }
    if prompt.len() > MAX_TEXT_BYTES {
        return Err("workflow attempt prompt exceeds 65536 UTF-8 bytes".to_string());
    }
    Ok(prompt)
}

fn random_id() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn secure_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|_| "create workflow transcript directory failed".to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "secure workflow transcript directory failed".to_string())
}

impl StepKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Handoff => "handoff",
            Self::Approval => "approval",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "agent" => Ok(Self::Agent),
            "handoff" => Ok(Self::Handoff),
            "approval" => Ok(Self::Approval),
            _ => Err(unavailable()),
        }
    }
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "paused" => Ok(Self::Paused),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            _ => Err(unavailable()),
        }
    }
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(unavailable()),
        }
    }
}

fn advance_ready(transaction: &Transaction<'_>, run_id: &str) -> Result<(), String> {
    let run_status: String = transaction
        .query_row(
            "SELECT status FROM workflow_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .map_err(|_| unavailable())?;
    if !matches!(run_status.as_str(), "running" | "waiting") {
        return Ok(());
    }
    let pending = {
        let mut statement = transaction
            .prepare(
                "SELECT id, needs_json FROM workflow_steps
                 WHERE run_id = ?1 AND status = 'pending' ORDER BY position",
            )
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| unavailable())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| unavailable())?
    };
    for (step_id, encoded) in pending {
        let needs: Vec<String> = serde_json::from_str(&encoded).map_err(|_| unavailable())?;
        let mut ready = true;
        for dependency in needs {
            let status: String = transaction
                .query_row(
                    "SELECT status FROM workflow_steps WHERE run_id = ?1 AND id = ?2",
                    params![run_id, dependency],
                    |row| row.get(0),
                )
                .map_err(|_| unavailable())?;
            if status != "succeeded" {
                ready = false;
                break;
            }
        }
        if ready {
            transaction
                .execute(
                    "UPDATE workflow_steps SET status = 'ready' WHERE run_id = ?1 AND id = ?2",
                    params![run_id, step_id],
                )
                .map_err(|_| unavailable())?;
            event(transaction, run_id, "step_ready", &step_id, "ready")?;
        }
    }
    Ok(())
}

fn settle_run(transaction: &Transaction<'_>, run_id: &str) -> Result<(), String> {
    let (unfinished, active, waiting, interrupted): (i64, i64, i64, i64) = transaction
        .query_row(
            "SELECT
                SUM(CASE WHEN status != 'succeeded' THEN 1 ELSE 0 END),
                SUM(CASE WHEN status IN ('ready','running') THEN 1 ELSE 0 END),
                SUM(CASE WHEN status = 'waiting' THEN 1 ELSE 0 END),
                SUM(CASE WHEN status = 'interrupted' THEN 1 ELSE 0 END)
             FROM workflow_steps WHERE run_id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|_| unavailable())?;
    if unfinished == 0 {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'succeeded', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_succeeded", "", "succeeded")?;
        return Ok(());
    }
    let status: String = transaction
        .query_row(
            "SELECT status FROM workflow_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .map_err(|_| unavailable())?;
    if active == 0 && interrupted > 0 && status == "running" {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'paused', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_paused", "", "paused")?;
    } else if active == 0 && waiting > 0 && status == "running" {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'waiting', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_waiting", "", "waiting")?;
    } else if active > 0 && status == "waiting" {
        transaction
            .execute(
                "UPDATE workflow_runs SET status = 'running', updated_at = ?2 WHERE id = ?1",
                params![run_id, timestamp()],
            )
            .map_err(|_| unavailable())?;
        event(transaction, run_id, "run_resumed", "", "running")?;
    }
    Ok(())
}

fn event(
    transaction: &Transaction<'_>,
    run_id: &str,
    kind: &str,
    step_id: &str,
    status: &str,
) -> Result<(), String> {
    transaction
        .execute(
            "INSERT INTO workflow_events (run_id, occurred_at, kind, step_id, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, timestamp(), kind, step_id, status],
        )
        .map_err(|_| unavailable())?;
    Ok(())
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn unavailable() -> String {
    "workflow store unavailable".to_string()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionFile {
    version: u32,
    #[serde(default)]
    description: String,
    steps: Vec<StepFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    id: String,
    #[serde(default)]
    kind: StepKind,
    #[serde(default)]
    agent: String,
    prompt: String,
    #[serde(default)]
    needs: Vec<String>,
}

pub(crate) fn parse_definition(
    name: &str,
    data: &[u8],
    has_agent: &dyn Fn(&str) -> bool,
) -> Result<Definition, String> {
    if !valid_name(name) {
        return Err(format!("workflow name {name:?} is invalid"));
    }
    if data.len() > MAX_DEFINITION_BYTES {
        return Err(format!("workflow {name} exceeds 1 MiB"));
    }
    let text = std::str::from_utf8(data).map_err(|_| format!("workflow {name} is not UTF-8"))?;
    let file: DefinitionFile =
        toml::from_str(text).map_err(|error| format!("workflow {name}: {error}"))?;
    if file.version != 1 {
        return Err(format!("workflow {name} version must be 1"));
    }
    if file.description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "workflow {name} description exceeds {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    if !(1..=MAX_STEPS).contains(&file.steps.len()) {
        return Err(format!(
            "workflow {name} must contain 1 to {MAX_STEPS} steps"
        ));
    }

    let mut seen = HashSet::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for raw in file.steps {
        if !valid_name(&raw.id) {
            return Err(format!("workflow {name} step id {:?} is invalid", raw.id));
        }
        if !seen.insert(raw.id.clone()) {
            return Err(format!(
                "workflow {name} contains duplicate step {:?}",
                raw.id
            ));
        }
        if raw.prompt.trim().is_empty() || raw.prompt.len() > MAX_TEXT_BYTES {
            return Err(format!(
                "workflow {name} step {:?} prompt must be 1 to 65536 UTF-8 bytes",
                raw.id
            ));
        }
        match raw.kind {
            StepKind::Agent | StepKind::Handoff => {
                if raw.agent.is_empty() || !has_agent(&raw.agent) {
                    return Err(format!(
                        "workflow {name} step {:?} references unknown agent {:?}",
                        raw.id, raw.agent
                    ));
                }
                if raw.kind == StepKind::Handoff && raw.needs.is_empty() {
                    return Err(format!(
                        "workflow {name} handoff step {:?} requires a dependency",
                        raw.id
                    ));
                }
            }
            StepKind::Approval if !raw.agent.is_empty() => {
                return Err(format!(
                    "workflow {name} approval step {:?} cannot set agent",
                    raw.id
                ));
            }
            StepKind::Approval => {}
        }
        let mut dependencies = HashSet::new();
        if raw
            .needs
            .iter()
            .any(|dependency| dependency == &raw.id || !dependencies.insert(dependency.clone()))
        {
            return Err(format!(
                "workflow {name} step {:?} has an invalid dependency",
                raw.id
            ));
        }
        steps.push(Step {
            id: raw.id,
            kind: raw.kind,
            agent: raw.agent,
            prompt: raw.prompt,
            needs: raw.needs,
        });
    }
    validate_graph(name, &steps)?;

    Ok(Definition {
        name: name.to_string(),
        description: file.description,
        steps,
        agents: Vec::new(),
        hash: format!("{:x}", Sha256::digest(data)),
    })
}

fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn validate_graph(name: &str, steps: &[Step]) -> Result<(), String> {
    let indexes: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(index, step)| (step.id.as_str(), index))
        .collect();
    let mut incoming = vec![0usize; steps.len()];
    let mut outgoing = vec![Vec::new(); steps.len()];
    for (index, step) in steps.iter().enumerate() {
        for dependency in &step.needs {
            let Some(&source) = indexes.get(dependency.as_str()) else {
                return Err(format!(
                    "workflow {name} step {:?} depends on unknown step {:?}",
                    step.id, dependency
                ));
            };
            incoming[index] += 1;
            outgoing[source].push(index);
        }
    }
    let mut ready: VecDeque<usize> = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for &target in &outgoing[index] {
            incoming[target] -= 1;
            if incoming[target] == 0 {
                ready.push_back(target);
            }
        }
    }
    if visited != steps.len() {
        return Err(format!("workflow {name} contains a dependency cycle"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    fn definition(source: &[u8]) -> Definition {
        let mut definition = parse_definition("flow", source, &|_| true).expect("definition");
        let names: HashSet<String> = definition
            .steps
            .iter()
            .filter(|step| matches!(step.kind, StepKind::Agent | StepKind::Handoff))
            .map(|step| step.agent.clone())
            .collect();
        definition.agents = names
            .into_iter()
            .map(|name| AgentSnapshot {
                name,
                ..AgentSnapshot::default()
            })
            .collect();
        definition
    }

    #[test]
    fn definition_accepts_a_bounded_dag() {
        let definition = parse_definition(
            "review-change",
            br#"
version = 1
description = "Review a change"

[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review it"
needs = ["research"]

[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship it?"
needs = ["review"]
"#,
            &|name| matches!(name, "researcher" | "reviewer"),
        )
        .expect("definition");

        assert_eq!(definition.name, "review-change");
        assert_eq!(definition.steps[0].kind, StepKind::Agent);
        assert_eq!(definition.steps[2].kind, StepKind::Approval);
        assert_eq!(definition.steps[2].needs, ["review"]);
        assert_eq!(definition.hash.len(), 64);
    }

    fn agent_snapshot(
        name: &str,
        tools: Option<&[&str]>,
        write_policy: SnapshotWritePolicy,
        write_paths: &[&str],
    ) -> AgentSnapshot {
        AgentSnapshot {
            name: name.to_string(),
            tools: tools.map(|tools| tools.iter().map(|tool| (*tool).to_string()).collect()),
            write_policy,
            write_paths: write_paths.iter().map(|path| (*path).to_string()).collect(),
            ..AgentSnapshot::default()
        }
    }

    #[test]
    fn write_coordination_rejects_parallel_global_writers() {
        let mut definition = definition(
            br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
"#,
        );
        definition.agents = vec![
            agent_snapshot(
                "left",
                Some(&["read", "edit"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
            agent_snapshot(
                "right",
                Some(&["write"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
        ];

        let error = validate_write_coordination(&definition).expect_err("parallel writers fail");

        assert!(error.contains("may run concurrently"), "{error}");
    }

    #[test]
    fn write_coordination_allows_readonly_parallel_and_serial_writers() {
        let mut serial = definition(
            br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
needs = ["left"]
[[steps]]
id = "plan"
agent = "planner"
prompt = "plan"
"#,
        );
        serial.agents = vec![
            agent_snapshot(
                "left",
                Some(&["edit"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
            agent_snapshot(
                "right",
                Some(&["write"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
            agent_snapshot(
                "planner",
                Some(&["read", "edit"]),
                SnapshotWritePolicy::ProposeOnly,
                &[],
            ),
        ];

        validate_write_coordination(&serial)
            .expect("serial writers and propose-only planner are ok");
    }

    #[test]
    fn write_coordination_allows_disjoint_owned_paths() {
        let mut definition = definition(
            br#"
version = 1
[[steps]]
id = "core"
agent = "core"
prompt = "core"
[[steps]]
id = "docs"
agent = "docs"
prompt = "docs"
"#,
        );
        definition.agents = vec![
            agent_snapshot(
                "core",
                Some(&["edit"]),
                SnapshotWritePolicy::OwnedPaths,
                &["crates/otto-core/**"],
            ),
            agent_snapshot(
                "docs",
                Some(&["write"]),
                SnapshotWritePolicy::OwnedPaths,
                &["docs/**"],
            ),
        ];

        validate_write_coordination(&definition).expect("disjoint owned paths are ok");
    }

    #[test]
    fn write_coordination_rejects_overlapping_owned_paths() {
        let mut definition = definition(
            br#"
version = 1
[[steps]]
id = "a"
agent = "a"
prompt = "a"
[[steps]]
id = "b"
agent = "b"
prompt = "b"
"#,
        );
        definition.agents = vec![
            agent_snapshot(
                "a",
                Some(&["edit"]),
                SnapshotWritePolicy::OwnedPaths,
                &["crates/otto/**"],
            ),
            agent_snapshot(
                "b",
                Some(&["write"]),
                SnapshotWritePolicy::OwnedPaths,
                &["crates/otto/src/**"],
            ),
        ];

        let error = validate_write_coordination(&definition).expect_err("overlap fails");

        assert!(error.contains("may run concurrently"), "{error}");
    }

    #[test]
    fn definition_accepts_handoff_to_a_known_agent() {
        let definition = parse_definition(
            "handoff-change",
            br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over and review it"
needs = ["research"]
"#,
            &|name| matches!(name, "researcher" | "reviewer"),
        )
        .expect("definition");

        assert_eq!(definition.steps[1].kind, StepKind::Handoff);
        assert_eq!(definition.steps[1].agent, "reviewer");
        assert_eq!(definition.steps[1].needs, ["research"]);
    }

    #[test]
    fn definition_rejects_a_root_handoff() {
        let error = parse_definition(
            "handoff",
            br#"
version = 1
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over"
"#,
            &|_| true,
        )
        .expect_err("root handoff");

        assert_eq!(
            error,
            "workflow handoff handoff step \"review\" requires a dependency"
        );
    }

    #[test]
    fn definition_rejects_cycles_before_execution() {
        let error = parse_definition(
            "cycle",
            br#"
version = 1
[[steps]]
id = "one"
agent = "worker"
prompt = "one"
needs = ["two"]
[[steps]]
id = "two"
agent = "worker"
prompt = "two"
needs = ["one"]
"#,
            &|_| true,
        )
        .expect_err("cycle");

        assert_eq!(error, "workflow cycle contains a dependency cycle");
    }

    #[test]
    fn definition_read_refuses_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("directory");
        let source = directory.path().join("source.toml");
        let link = directory.path().join("link.toml");
        std::fs::write(&source, "version = 1").expect("write");
        symlink(&source, &link).expect("symlink");
        assert!(read_definition_file(&link).is_err());
    }

    #[test]
    fn store_commits_dependencies_and_terminal_run() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        let run = store
            .create_run(
                "run-1",
                &definition,
                "/workspace",
                "default",
                "openai-compatible",
                "model",
                "input",
            )
            .expect("create");
        assert_eq!(run.steps[0].status, StepStatus::Ready);
        assert_eq!(run.steps[1].status, StepStatus::Pending);

        let attempt = store
            .claim_ready("run-1", "first", "/tmp/first.jsonl")
            .expect("claim");
        store
            .finish_attempt("run-1", "first", attempt, Ok("result one"))
            .expect("finish first");
        let run = store.get_run("run-1").expect("get").expect("run");
        assert_eq!(run.steps[1].status, StepStatus::Ready);

        let attempt = store
            .claim_ready("run-1", "second", "/tmp/second.jsonl")
            .expect("claim second");
        store
            .finish_attempt("run-1", "second", attempt, Ok("done"))
            .expect("finish second");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Succeeded
        );
    }

    #[test]
    fn recovery_pauses_running_attempt_without_retrying_it() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        store
            .claim_ready("run-1", "work", "/tmp/work.jsonl")
            .expect("claim");
        store
            .create_run("run-2", &definition, "/other", "p", "provider", "m", "")
            .expect("other create");
        store
            .claim_ready("run-2", "work", "/tmp/other.jsonl")
            .expect("other claim");

        assert_eq!(store.recover("/w").expect("recover"), 1);
        let run = store.get_run("run-1").expect("get").expect("run");
        assert_eq!(run.status, RunStatus::Paused);
        assert_eq!(run.steps[0].status, StepStatus::Interrupted);
        assert_eq!(run.steps[0].attempt, 1);
        assert_eq!(
            store.get_run("run-2").expect("get").expect("other").status,
            RunStatus::Running
        );
    }

    #[test]
    fn manual_retry_creates_a_new_attempt_and_keeps_the_old_transcript() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        store
            .claim_ready("run-1", "work", "/tmp/attempt-1.jsonl")
            .expect("first");
        store.recover("/w").expect("recover");
        store.retry("run-1", "work").expect("retry");
        assert_eq!(
            store
                .claim_ready("run-1", "work", "/tmp/attempt-2.jsonl")
                .expect("second"),
            2
        );
        let connection = store.connection.lock().expect("connection");
        let mut statement = connection
            .prepare(
                "SELECT transcript_path FROM workflow_attempts
                 WHERE run_id = 'run-1' AND step_id = 'work' ORDER BY attempt",
            )
            .expect("query");
        let paths: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("paths");
        assert_eq!(paths, ["/tmp/attempt-1.jsonl", "/tmp/attempt-2.jsonl"]);
    }

    #[test]
    fn approval_response_is_durable_and_idempotent() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship?"
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        let request = store
            .request_approval("request-1", "run-1", "approve")
            .expect("request");
        assert_eq!(request.status, "pending");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Waiting
        );

        store.respond("request-1", true).expect("approve");
        store.respond("request-1", true).expect("same response");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Succeeded
        );
        assert_eq!(
            store.respond("request-1", false).expect_err("conflict"),
            "approval request already answered differently"
        );
    }

    #[test]
    fn handoff_prompt_marks_the_transfer() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "research"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "review"
needs = ["research"]
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "input")
            .expect("create");
        let attempt = store
            .claim_ready("run-1", "research", "/tmp/research.jsonl")
            .expect("claim");
        store
            .finish_attempt("run-1", "research", attempt, Ok("facts"))
            .expect("finish");
        let run = store.get_run("run-1").expect("get").expect("run");
        let prompt = attempt_prompt(&run, &run.steps[1]).expect("prompt");

        assert!(prompt.contains("## Handoff"));
        assert!(prompt.contains("You are receiving control from: research"));
    }

    struct FakeExecutor;

    #[async_trait::async_trait]
    impl Executor for FakeExecutor {
        async fn execute(
            &self,
            attempt: Attempt,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            Ok(format!("{} complete", attempt.step_id))
        }
    }

    #[tokio::test]
    async fn controller_runs_sequential_steps_to_completion() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog {
                definitions: vec![definition],
            },
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity {
                profile: "default".into(),
                provider: "openai-compatible".into(),
                model: "model".into(),
            },
            2,
        );

        let run = controller.start("flow", "request").await.expect("start");
        let completed = controller.wait(&run.id).await.expect("wait");
        assert_eq!(completed.status, RunStatus::Succeeded);
        assert_eq!(completed.steps[0].result, "first complete");
        assert_eq!(completed.steps[1].result, "second complete");
    }

    #[tokio::test]
    async fn controller_runs_handoff_steps_to_completion() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "research"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "review"
needs = ["research"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog::from_definitions(vec![definition]),
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
        );

        let run = controller.start("flow", "").await.expect("start");
        let completed = controller.wait(&run.id).await.expect("wait");
        assert_eq!(completed.status, RunStatus::Succeeded);
        assert_eq!(completed.steps[1].kind, StepKind::Handoff);
        assert_eq!(completed.steps[1].result, "review complete");
    }

    #[tokio::test]
    async fn controller_waits_for_parallel_approval_then_resumes_dependents() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship?"
[[steps]]
id = "deliver"
agent = "worker"
prompt = "deliver"
needs = ["work", "approve"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog::from_definitions(vec![definition]),
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
        );

        let run = controller.start("flow", "").await.expect("start");
        let waiting = controller.wait(&run.id).await.expect("wait");
        assert_eq!(waiting.status, RunStatus::Waiting);
        let request = controller
            .requests(&run.id)
            .expect("requests")
            .pop()
            .expect("request");
        let resumed = controller.respond(&request.id, true).expect("approve");
        assert_eq!(resumed.status, RunStatus::Running);
        let completed = controller.wait(&run.id).await.expect("complete");
        assert_eq!(completed.status, RunStatus::Succeeded);
        assert_eq!(completed.steps[2].result, "deliver complete");
    }

    struct FailAndCancelExecutor {
        sibling_stopped: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Executor for FailAndCancelExecutor {
        async fn execute(
            &self,
            attempt: Attempt,
            cancel: &CancellationToken,
        ) -> Result<String, String> {
            if attempt.step_id == "fail" {
                return Err("failed".to_string());
            }
            cancel.cancelled().await;
            self.sibling_stopped.store(true, Ordering::SeqCst);
            Err("context canceled".to_string())
        }
    }

    #[tokio::test]
    async fn failure_waits_for_running_siblings_and_never_schedules_dependents() {
        let stopped = Arc::new(AtomicBool::new(false));
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "fail"
agent = "worker"
prompt = "fail"
[[steps]]
id = "sibling"
agent = "worker"
prompt = "wait"
[[steps]]
id = "downstream"
agent = "worker"
prompt = "never"
needs = ["sibling"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(FailAndCancelExecutor {
                sibling_stopped: Arc::clone(&stopped),
            }),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
        );

        let run = controller.start("flow", "").await.expect("start");
        let failed = controller.wait(&run.id).await.expect("wait");
        assert!(stopped.load(Ordering::SeqCst));
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.steps[2].status, StepStatus::Canceled);
    }

    struct CountingExecutor(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Executor for CountingExecutor {
        async fn execute(
            &self,
            attempt: Attempt,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(format!("{} done", attempt.step_id))
        }
    }

    #[tokio::test]
    async fn resume_schedules_a_committed_ready_step_once() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        store
            .create_run("run-1", &definition, "/workspace", "", "", "", "")
            .expect("create");
        let attempt = store
            .claim_ready("run-1", "first", "/tmp/first")
            .expect("claim");
        store
            .finish_attempt("run-1", "first", attempt, Ok("first done"))
            .expect("finish");
        let calls = Arc::new(AtomicUsize::new(0));
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog::from_definitions(vec![definition]),
            Arc::new(CountingExecutor(Arc::clone(&calls))),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            1,
        );

        controller.resume("run-1", None).await.expect("resume");
        let done = controller.wait("run-1").await.expect("wait");
        assert_eq!(done.status, RunStatus::Succeeded);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct BlockingExecutor {
        started: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl Executor for BlockingExecutor {
        async fn execute(
            &self,
            _attempt: Attempt,
            cancel: &CancellationToken,
        ) -> Result<String, String> {
            self.started.notify_one();
            cancel.cancelled().await;
            Err("context canceled".to_string())
        }
    }

    #[tokio::test]
    async fn graceful_close_persists_an_interrupted_attempt() {
        let started = Arc::new(Notify::new());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(BlockingExecutor {
                started: Arc::clone(&started),
            }),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            1,
        );
        let run = controller.start("flow", "").await.expect("start");
        started.notified().await;

        controller.close().await;
        let paused = controller.get(&run.id).expect("run");
        assert_eq!(paused.status, RunStatus::Paused);
        assert_eq!(paused.steps[0].status, StepStatus::Interrupted);
    }
}
