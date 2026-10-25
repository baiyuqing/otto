//! Cross-domain migration references. Only verified journal aliases admit paths.
use crate::storage::backends::posix::journal_migration::JournalStage;
use crate::storage::migration::{contracts::*, group::ReferenceClosure};
use crate::storage::{Connection, OptionalExtension};
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub struct NativeReferenceClosure {
    databases: BTreeMap<String, PathBuf>,
    journals: Option<Arc<JournalStage>>,
}
impl NativeReferenceClosure {
    pub fn new(databases: BTreeMap<String, PathBuf>, journals: Option<Arc<JournalStage>>) -> Self {
        Self {
            databases,
            journals,
        }
    }
}
#[async_trait]
impl ReferenceClosure for NativeReferenceClosure {
    async fn validate(&self, manifest: &Manifest) -> Result<()> {
        let sql_domains: BTreeSet<_> = manifest
            .source
            .domains
            .keys()
            .filter(|domain| domain.as_str() != "sessions" && domain.as_str() != "artifacts")
            .cloned()
            .collect();
        if self.databases.keys().cloned().collect::<BTreeSet<_>>() != sql_domains {
            return Err(invalid(
                "cross-domain verification requires every SQL family",
            ));
        }
        if manifest.source.domains.contains_key("sessions") != self.journals.is_some() {
            return Err(invalid(
                "session journal stage missing or outside migration group",
            ));
        }
        let (aliases, session_ids) = if let Some(journals) = &self.journals {
            let dataset = journals.reference_dataset(manifest).await?;
            if dataset.migration_id != manifest.migration_id || dataset.version != 1 {
                return Err(invalid("journal reference generation mismatch"));
            }
            (dataset.aliases, dataset.session_ids)
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        let databases = self.databases.clone();
        tokio::task::spawn_blocking(move || {
            validate_reference_closure(&databases, &aliases, &session_ids)
        })
        .await
        .map_err(|_| invalid("native reference verification worker unavailable"))?
    }
}

pub fn validate_reference_closure(
    databases: &BTreeMap<String, PathBuf>,
    aliases: &BTreeMap<String, String>,
    session_ids: &BTreeMap<String, String>,
) -> Result<()> {
    for relative in aliases.values().chain(session_ids.values()) {
        safe_relative(relative)?;
    }
    let mut connections = BTreeMap::new();
    for (domain, path) in databases {
        if !matches!(
            domain.as_str(),
            "approvals"
                | "workflows"
                | "schedule"
                | "usage"
                | "tasks"
                | "skill_checks"
                | "memory_advice"
        ) {
            return Err(invalid("unsupported SQL reference family"));
        }
        if std::fs::symlink_metadata(path)
            .map_err(|_| invalid("staged SQL family unavailable"))?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("staged SQL family cannot be a symlink"));
        }
        connections.insert(
            domain.as_str(),
            Connection::open_read_only(path).map_err(sql_error)?,
        );
    }
    if let Some(workflows) = connections.get("workflows") {
        workflow_references(workflows, aliases)?;
    }
    if let Some(tasks) = connections.get("tasks") {
        task_references(tasks, aliases, session_ids)?;
    }
    if let Some(schedule) = connections.get("schedule") {
        session_column(
            schedule,
            "items",
            "session_id",
            "state IN ('waiting','ready')",
            session_ids,
        )?;
        session_column(
            schedule,
            "deliveries",
            "session_id",
            "appended=0",
            session_ids,
        )?;
    }
    if let Some(approvals) = connections.get("approvals") {
        approval_references(approvals, &connections, session_ids)?;
    }
    Ok(())
}
fn safe_relative(value: &str) -> Result<()> {
    if value.is_empty()
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid(
            "journal alias target is not a confined relative path",
        ));
    }
    Ok(())
}
fn path_reference(path: &str, aliases: &BTreeMap<String, String>) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    if Path::new(path)
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("journal reference escapes its admitted path"));
    }
    aliases
        .get(path)
        .ok_or_else(|| invalid("linked transcript was not included in the migration"))?;
    Ok(())
}
fn session_reference(id: &str, sessions: &BTreeMap<String, String>) -> Result<()> {
    if id.is_empty() || id.starts_with("memory:") {
        return Ok(());
    }
    if !sessions.contains_key(id) {
        return Err(invalid("linked session was not included in the migration"));
    }
    Ok(())
}
fn workflow_references(connection: &Connection, aliases: &BTreeMap<String, String>) -> Result<()> {
    let broken: i64 = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM workflow_runs r LEFT JOIN workflow_runs p \
        ON p.id=r.forked_from_run_id LEFT JOIN workflow_events e ON \
        e.seq=r.forked_from_event_seq AND e.run_id=r.forked_from_run_id WHERE \
        r.forked_from_run_id IS NOT NULL AND (p.id IS NULL OR e.seq IS NULL OR \
        e.step_id<>r.forked_from_step_id))+(SELECT COUNT(*) FROM workflow_steps \
        s LEFT JOIN workflow_steps p ON p.run_id=s.source_run_id AND \
        p.id=s.source_step_id LEFT JOIN workflow_attempts a ON \
        a.run_id=s.source_run_id AND a.step_id=s.source_step_id AND \
        a.attempt=s.source_attempt WHERE s.source_run_id IS NOT NULL AND (p.id \
        IS NULL OR (s.source_attempt>0 AND a.attempt IS NULL)))+(SELECT \
        COUNT(*) FROM workflow_attempts a LEFT JOIN workflow_steps s ON \
        s.run_id=a.run_id AND s.id=a.step_id WHERE s.id IS NULL)",
            (),
            |r| r.get(0),
        )
        .map_err(sql_error)?;
    if broken != 0 {
        return Err(invalid("workflow attempt/fork provenance is incomplete"));
    }
    for table in ["workflow_steps", "workflow_attempts"] {
        bounded_columns(connection, table, &["transcript_path"])?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT transcript_path FROM {table} WHERE transcript_path<>''"
            ))
            .map_err(sql_error)?;
        let mut rows = statement.query(()).map_err(sql_error)?;
        while let Some(row) = rows.next().map_err(sql_error)? {
            path_reference(&row.get::<String>(0).map_err(sql_error)?, aliases)?;
        }
    }
    Ok(())
}
fn task_references(
    connection: &Connection,
    aliases: &BTreeMap<String, String>,
    sessions: &BTreeMap<String, String>,
) -> Result<()> {
    bounded_columns(
        connection,
        "tasks",
        &["parent_session", "parent_session_path", "session_path"],
    )?;
    let mut statement = connection
        .prepare("SELECT parent_session,parent_session_path,session_path,status FROM tasks")
        .map_err(sql_error)?;
    let mut rows = statement.query(()).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let parent: String = row.get(0).map_err(sql_error)?;
        session_reference(&parent, sessions)?;
        let parent_path = row.get::<String>(1).map_err(sql_error)?;
        path_reference(&parent_path, aliases)?;
        if !parent.starts_with("memory:")
            && !parent_path.is_empty()
            && sessions.get(&parent) != aliases.get(&parent_path)
        {
            return Err(invalid("task parent session identity/path differ"));
        }
        path_reference(&row.get::<String>(2).map_err(sql_error)?, aliases)?;
        let status: String = row.get(3).map_err(sql_error)?;
        if matches!(status.as_str(), "queued" | "running") {
            return Err(invalid("task migration retained live process authority"));
        }
    }
    Ok(())
}
fn approval_references(
    connection: &Connection,
    databases: &BTreeMap<&str, Connection>,
    sessions: &BTreeMap<String, String>,
) -> Result<()> {
    bounded_columns(connection, "approvals", &["body"])?;
    let mut statement = connection
        .prepare("SELECT body FROM approvals")
        .map_err(sql_error)?;
    let mut rows = statement.query(()).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let body: String = row.get(0).map_err(sql_error)?;
        let request = crate::storage::backends::sqlite::approval::decode_persisted(&body)
            .map_err(|_| invalid("invalid linked approval record"))?;
        use otto_core::approval::{ApplicationState, State};
        if request.state != State::Pending
            && !matches!(
                request.application,
                ApplicationState::NotStarted | ApplicationState::Applying
            )
        {
            continue;
        }
        session_reference(&request.scope.session_id, sessions)?;
        match request.subject.source.as_str() {
            "workflow" => workflow_approval(
                &request,
                databases.get("workflows").ok_or_else(|| {
                    invalid("live workflow approval requires the workflow family")
                })?,
            )?,
            // Memory candidates are files under the source home's
            // `.otto/memory`, which a transfer does not move.
            "memory" => {}
            "conversation"
                if request.kind == otto_core::approval::Kind::Confirmation
                    && request.application == ApplicationState::NotRequired => {}
            _ => {
                return Err(invalid(
                    "unsupported live approval subject; resolve it before migration",
                ));
            }
        }
    }
    Ok(())
}
fn workflow_approval(
    request: &otto_core::approval::Request,
    connection: &Connection,
) -> Result<()> {
    bounded_columns(connection, "workflow_requests", &["prompt"])?;
    let target = connection
        .query_row(
            "SELECT q.run_id,q.prompt,q.status,r.workspace,s.kind FROM \
        workflow_requests q JOIN workflow_runs r ON r.id=q.run_id JOIN \
        workflow_steps s ON s.run_id=q.run_id AND s.id=q.step_id WHERE q.id=?1",
            [request.subject.id.as_str()],
            |r| {
                Ok((
                    r.get::<String>(0)?,
                    r.get::<String>(1)?,
                    r.get::<String>(2)?,
                    r.get::<String>(3)?,
                    r.get::<String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| invalid("live workflow approval target is missing"))?;
    if target.0 != request.scope.run_id
        || target.3 != request.scope.workspace
        || target.4 != "approval"
        || crate::approval::digest(&target.1) != request.subject.version
    {
        return Err(invalid(
            "live workflow approval target identity/version differs",
        ));
    }
    let expected = if request.state == otto_core::approval::State::Approved {
        "approved"
    } else {
        "rejected"
    };
    if target.2 != "pending"
        && (request.state == otto_core::approval::State::Pending || target.2 != expected)
    {
        return Err(invalid(
            "live workflow approval has a conflicting target receipt",
        ));
    }
    Ok(())
}
fn session_column(
    connection: &Connection,
    table: &str,
    column: &str,
    predicate: &str,
    sessions: &BTreeMap<String, String>,
) -> Result<()> {
    bounded_columns(connection, table, &[column])?;
    let mut statement = connection
        .prepare(&format!("SELECT {column} FROM {table} WHERE {predicate}"))
        .map_err(sql_error)?;
    let mut rows = statement.query(()).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        session_reference(&row.get::<String>(0).map_err(sql_error)?, sessions)?;
    }
    Ok(())
}
fn bounded_columns(connection: &Connection, table: &str, columns: &[&str]) -> Result<()> {
    let oversized = columns
        .iter()
        .map(|column| format!("length(CAST({column} AS BLOB))>1048576"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let count: i64 = connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {oversized}"),
            (),
            |r| r.get(0),
        )
        .map_err(sql_error)?;
    if count != 0 {
        return Err(invalid(
            "linked domain reference exceeds verification bound",
        ));
    }
    Ok(())
}
fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn sql_error(_: crate::storage::Error) -> Error {
    Error::Unavailable("staged SQL reference verification unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::backends::sqlite::workflow::SqliteWorkflowStore;
    use crate::workflow::{Definition, Step, StepKind, repository::WorkflowStore};
    use otto_core::approval::ApprovalMode;
    #[test]
    fn linked_workflow_approval_and_transcript_require_the_actual_included_state() {
        let dir = tempfile::tempdir().unwrap();
        let approvals_path = dir.path().join("approvals.db");
        let workflow_path = dir.path().join("workflows.db");
        let workflows = SqliteWorkflowStore::open(&workflow_path).unwrap();
        workflows
            .create_run(
                "run",
                &Definition {
                    source: String::new(),
                    name: "flow".into(),
                    description: String::new(),
                    steps: vec![Step {
                        id: "review".into(),
                        kind: StepKind::Approval,
                        agent: String::new(),
                        prompt: "review operation".into(),
                        needs: vec![],
                    }],
                    agents: vec![],
                    hash: "hash".into(),
                },
                "/w",
                "default",
                "openai-compatible",
                "m",
                "",
            )
            .unwrap();
        let pending = workflows
            .request_approval("review-request", "run", "review")
            .unwrap();
        let approvals = crate::approval::Service::open(&approvals_path).unwrap();
        let request = crate::approval::workflow::publish(&approvals, "/w", &pending).unwrap();
        let mut databases = BTreeMap::from([("approvals".into(), approvals_path)]);
        assert!(
            validate_reference_closure(&databases, &BTreeMap::new(), &BTreeMap::new()).is_err()
        );
        databases.insert("workflows".into(), workflow_path.clone());
        assert!(validate_reference_closure(&databases, &BTreeMap::new(), &BTreeMap::new()).is_ok());
        let connection = Connection::open(&workflow_path).unwrap();
        connection
            .execute(
                "UPDATE workflow_steps SET transcript_path='/old/session.jsonl'",
                (),
            )
            .unwrap();
        assert!(
            validate_reference_closure(&databases, &BTreeMap::new(), &BTreeMap::new()).is_err()
        );
        let aliases =
            BTreeMap::from([("/old/session.jsonl".into(), "sessions/child.jsonl".into())]);
        let sessions = BTreeMap::from([("child".into(), "sessions/child.jsonl".into())]);
        assert!(validate_reference_closure(&databases, &aliases, &sessions).is_ok());
        connection
            .execute(
                "UPDATE workflow_requests SET prompt='changed operation'",
                (),
            )
            .unwrap();
        assert!(validate_reference_closure(&databases, &aliases, &sessions).is_err());
        // Completed history remains readable even when a live target is gone.
        approvals
            .decide(
                &request.id,
                &request.scope,
                1,
                true,
                "",
                &crate::approval::Actor::local("test"),
            )
            .unwrap();
        approvals
            .application(
                &request.id,
                &request.scope,
                otto_core::approval::ApplicationState::NotStarted,
                otto_core::approval::ApplicationState::Applying,
                "",
            )
            .unwrap();
        approvals
            .application(
                &request.id,
                &request.scope,
                otto_core::approval::ApplicationState::Applying,
                otto_core::approval::ApplicationState::Applied,
                "",
            )
            .unwrap();
        databases.remove("workflows");
        assert!(validate_reference_closure(&databases, &BTreeMap::new(), &BTreeMap::new()).is_ok());
        let scope = otto_core::approval::Scope {
            workspace: "/w".into(),
            session_id: "child".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        approvals.set_mode(&scope, ApprovalMode::Manual).unwrap();
        approvals
            .create(crate::approval::Proposal {
                kind: otto_core::approval::Kind::Confirmation,
                authority: otto_core::approval::Authority::Consent,
                scope,
                subject: otto_core::approval::Subject {
                    tool_call_id: String::new(),
                    source: "conversation".into(),
                    id: "review-plan".into(),
                    version: "1".into(),
                    details: "Review this plan.".into(),
                },
                title: "Review plan".into(),
                reason: "scope".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        assert!(
            validate_reference_closure(&databases, &BTreeMap::new(), &BTreeMap::new()).is_err()
        );
        assert!(validate_reference_closure(&databases, &aliases, &sessions).is_ok());
    }
}
