//! Cross-domain verification over logical records; no backend, SQL or filesystem I/O.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::group::ReferenceClosure;
use super::record::{LogicalRecord, RecordSchema};
use super::{Error, JournalReferences, Manifest, Record, Result, StageImport, chunk_hash};

pub struct PortableReferenceClosure {
    pub stages: BTreeMap<String, Arc<dyn StageImport>>,
    pub max_bytes: usize,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

pub fn schemas(domain: &str) -> Result<&'static [RecordSchema]> {
    Ok(match domain {
        "workflows" => crate::workflow::migration::RECORDS,
        "schedule" => crate::schedule::migration::RECORDS,
        "usage" => crate::usage::migration::RECORDS,
        "tasks" => crate::subagent::migration::RECORDS,
        "reflection" => crate::reflection::migration::RECORDS,
        "skill_checks" => crate::skill::check_migration::RECORDS,
        "memory_advice" => crate::memory::advice::migration::RECORDS,
        _ => return Err(invalid("unsupported logical record family")),
    })
}

#[async_trait]
impl ReferenceClosure for PortableReferenceClosure {
    async fn validate(&self, manifest: &Manifest) -> Result<()> {
        if self.max_bytes == 0 || self.stages.keys().ne(manifest.source.domains.keys()) {
            return Err(invalid(
                "reference verification requires the complete domain group",
            ));
        }
        let mut records = BTreeMap::new();
        let mut remaining = self.max_bytes;
        let mut journals = JournalReferences::default();
        for (domain, stage) in &self.stages {
            if domain == "sessions" {
                let mut scoped = manifest.clone();
                scoped.source.domains.retain(|name, _| name == domain);
                scoped.progress.retain(|name, _| name == domain);
                journals = stage.journal_references(&scoped).await?;
                if journals.migration_id != manifest.migration_id {
                    return Err(invalid("journal reference generation differs"));
                }
                continue;
            }
            let mut cursor = String::new();
            let mut seen = BTreeSet::new();
            loop {
                if !seen.insert(cursor.clone()) {
                    return Err(invalid("non-progressing verification cursor"));
                }
                let chunk = stage
                    .verification_chunk(domain, &cursor, manifest.max_chunk_bytes)
                    .await?;
                if chunk.cursor != cursor || !chunk.blobs.is_empty() {
                    return Err(invalid("invalid logical verification chunk"));
                }
                chunk_hash(&chunk, manifest.max_chunk_bytes)?;
                remaining = remaining
                    .checked_sub(
                        serde_json::to_vec(&chunk)
                            .map_err(|_| invalid("verification encoding failed"))?
                            .len(),
                    )
                    .ok_or_else(|| {
                        invalid("reference verification exceeds its declared memory bound")
                    })?;
                for record in chunk.records {
                    if domain != "approvals" {
                        LogicalRecord::decode(&record, schemas(domain)?)?;
                    }
                    if records
                        .insert((domain.clone(), record.id.clone()), record)
                        .is_some()
                    {
                        return Err(invalid("duplicate logical verification identity"));
                    }
                }
                match chunk.next_cursor {
                    Some(next) => cursor = next,
                    None => break,
                }
            }
        }
        validate_records(&records, &journals)
    }
}

fn field<'a>(record: &'a LogicalRecord, name: &str) -> Result<&'a Value> {
    record
        .fields
        .get(name)
        .ok_or_else(|| invalid("missing logical reference field"))
}
fn text<'a>(record: &'a LogicalRecord, name: &str) -> Result<&'a str> {
    field(record, name)?
        .as_str()
        .ok_or_else(|| invalid("invalid logical text reference"))
}
fn integer(record: &LogicalRecord, name: &str) -> Result<i64> {
    field(record, name)?
        .as_i64()
        .ok_or_else(|| invalid("invalid logical integer reference"))
}
fn lookup<'a>(
    records: &'a BTreeMap<String, LogicalRecord>,
    kind: &str,
    keys: &[Value],
) -> Result<&'a LogicalRecord> {
    let id = format!(
        "{kind}:{}",
        serde_json::to_string(keys).map_err(|_| invalid("invalid reference identity"))?
    );
    records
        .get(&id)
        .ok_or_else(|| invalid("linked logical record is missing"))
}
fn session(id: &str, journals: &JournalReferences) -> Result<()> {
    if !id.is_empty() && !id.starts_with("memory:") && !journals.session_ids.contains_key(id) {
        return Err(invalid("linked session was not included in the migration"));
    }
    Ok(())
}
fn transcript(path: &str, journals: &JournalReferences) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    if Path::new(path)
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        || !journals.aliases.contains_key(path)
    {
        return Err(invalid(
            "linked transcript was not included in the migration",
        ));
    }
    Ok(())
}

pub fn validate_records(
    records: &BTreeMap<(String, String), Record>,
    journals: &JournalReferences,
) -> Result<()> {
    for path in journals
        .aliases
        .values()
        .chain(journals.session_ids.values())
    {
        if path.is_empty()
            || Path::new(path)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(invalid("journal alias is not a confined relative path"));
        }
    }
    let mut logical = BTreeMap::new();
    let mut approvals = Vec::new();
    for ((domain, id), record) in records {
        if domain == "approvals" {
            let approval: crate::approval::migration::ApprovalRecord =
                serde_json::from_slice(&record.payload)
                    .map_err(|_| invalid("invalid approval migration record"))?;
            let (expected, revision) = approval.metadata()?;
            if &expected != id || revision != record.revision {
                return Err(invalid("approval migration identity differs"));
            }
            if let crate::approval::migration::ApprovalRecord::Request { request, .. } = approval {
                approvals.push(
                    crate::approval::codec::decode_persisted(&request)
                        .map_err(|_| invalid("invalid linked approval authority"))?,
                );
            }
        } else {
            let record = LogicalRecord::decode(record, schemas(domain)?)?;
            if logical.insert(id.clone(), record).is_some() {
                return Err(invalid("logical record identity collision"));
            }
        }
    }
    for record in logical.values() {
        match record.kind.as_str() {
            "workflow_run" if !field(record, "forked_from_run_id")?.is_null() => {
                let source = field(record, "forked_from_run_id")?.clone();
                lookup(&logical, "workflow_run", std::slice::from_ref(&source))?;
                let event = lookup(
                    &logical,
                    "workflow_event",
                    &[field(record, "forked_from_event_seq")?.clone()],
                )?;
                if field(event, "run_id")? != &source
                    || field(event, "step_id")? != field(record, "forked_from_step_id")?
                {
                    return Err(invalid("workflow fork provenance differs"));
                }
            }
            "workflow_step" | "workflow_attempt" => {
                transcript(text(record, "transcript_path")?, journals)?;
                if record.kind == "workflow_attempt" {
                    lookup(
                        &logical,
                        "workflow_step",
                        &[
                            field(record, "run_id")?.clone(),
                            field(record, "step_id")?.clone(),
                        ],
                    )?;
                } else if !field(record, "source_run_id")?.is_null() {
                    let run = field(record, "source_run_id")?.clone();
                    let step = field(record, "source_step_id")?.clone();
                    lookup(&logical, "workflow_step", &[run.clone(), step.clone()])?;
                    if integer(record, "source_attempt")? > 0 {
                        lookup(
                            &logical,
                            "workflow_attempt",
                            &[run, step, field(record, "source_attempt")?.clone()],
                        )?;
                    }
                }
            }
            "subagent_task" => {
                let parent = text(record, "parent_session")?;
                let parent_path = text(record, "parent_session_path")?;
                session(parent, journals)?;
                transcript(parent_path, journals)?;
                transcript(text(record, "session_path")?, journals)?;
                if !parent.starts_with("memory:")
                    && !parent_path.is_empty()
                    && journals.session_ids.get(parent) != journals.aliases.get(parent_path)
                {
                    return Err(invalid("task parent session identity and path differ"));
                }
                if matches!(text(record, "status")?, "queued" | "running") {
                    return Err(invalid("task migration retained live process authority"));
                }
            }
            "reflection_watermark" => session(text(record, "session_id")?, journals)?,
            "schedule_occurrence" if matches!(text(record, "state")?, "waiting" | "ready") => {
                session(text(record, "session_id")?, journals)?
            }
            "schedule_delivery_receipt" if integer(record, "appended")? == 0 => {
                session(text(record, "session_id")?, journals)?
            }
            _ => (),
        }
    }
    for request in approvals {
        use otto_core::approval::{ApplicationState, State};
        if request.state != State::Pending
            && !matches!(
                request.application,
                ApplicationState::NotStarted | ApplicationState::Applying
            )
        {
            continue;
        }
        session(&request.scope.session_id, journals)?;
        match request.subject.source.as_str() {
            "workflow" => {
                let target = lookup(
                    &logical,
                    "workflow_review_request",
                    &[json!(request.subject.id)],
                )?;
                let run = lookup(
                    &logical,
                    "workflow_run",
                    &[field(target, "run_id")?.clone()],
                )?;
                let step = lookup(
                    &logical,
                    "workflow_step",
                    &[
                        field(target, "run_id")?.clone(),
                        field(target, "step_id")?.clone(),
                    ],
                )?;
                if text(target, "run_id")? != request.scope.run_id
                    || text(run, "workspace")? != request.scope.workspace
                    || text(step, "kind")? != "approval"
                    || crate::approval::digest(text(target, "prompt")?) != request.subject.version
                {
                    return Err(invalid(
                        "live workflow approval identity or version differs",
                    ));
                }
                let expected = if request.state == State::Approved {
                    "approved"
                } else {
                    "rejected"
                };
                if text(target, "status")? != "pending"
                    && (request.state == State::Pending || text(target, "status")? != expected)
                {
                    return Err(invalid("workflow approval receipt conflicts"));
                }
            }
            // Memory candidates are files under the source home's
            // `.otto/memory`, which a migration does not move.
            "memory" => {}
            "conversation"
                if request.kind == otto_core::approval::Kind::Confirmation
                    && request.application == ApplicationState::NotRequired => {}
            _ => {
                return Err(invalid(
                    "unsupported live approval subject; resolve before migration",
                ));
            }
        }
    }
    Ok(())
}
