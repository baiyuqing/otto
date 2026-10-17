//! Public behavioral checks for isolated adapter test namespaces.
//! These checks mutate supplied fixtures; real durability/fault gates remain backend-owned.
use crate::approval::repository::{ApprovalStore, ClaimChange};
use crate::storage::migration::{Chunk, Error, Result, StageImport, chunk_hash};

fn ensure(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Invalid(message.into()))
    }
}
fn unavailable<E>(_: E) -> Error {
    Error::Unavailable("contract operation failed".into())
}

pub fn approvals(
    store: &dyn ApprovalStore,
    key: &str,
    expected: &otto_core::approval::Request,
    next: &otto_core::approval::Request,
) -> Result<()> {
    let (created, inserted) = store.create(key, expected).map_err(unavailable)?;
    ensure(
        inserted && &created == expected,
        "approval creation differs",
    )?;
    let (duplicate, inserted) = store.create(key, expected).map_err(unavailable)?;
    ensure(
        !inserted && duplicate == created,
        "approval subject replay differs",
    )?;
    let committed = store
        .commit(expected, next, ClaimChange::None, false)
        .map_err(unavailable)?;
    ensure(
        &committed == next && store.load(&next.id).map_err(unavailable)? == *next,
        "approval commit differs",
    )?;
    ensure(
        matches!(
            store.commit(expected, next, ClaimChange::None, false),
            Err(crate::approval::Error::Conflict)
        ),
        "stale approval mutation was accepted",
    )?;
    let events = store
        .events(&next.scope.workspace, Some(&next.scope.session_id), 0)
        .map_err(unavailable)?;
    ensure(
        events.len() == 2
            && events[0].request == *expected
            && events[1].request == *next
            && events[0].sequence < events[1].sequence,
        "approval request and event atomicity differs",
    )
}

pub fn approval_race(
    first: std::sync::Arc<dyn ApprovalStore>,
    second: std::sync::Arc<dyn ApprovalStore>,
    expected: &otto_core::approval::Request,
    next: &otto_core::approval::Request,
) -> Result<()> {
    first.create("race", expected).map_err(unavailable)?;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [first.clone(), second]
        .into_iter()
        .map(|store| {
            let barrier = barrier.clone();
            let expected = expected.clone();
            let next = next.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.commit(&expected, &next, ClaimChange::None, false)
            })
        })
        .collect();
    let outcomes: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().map_err(unavailable))
        .collect::<Result<_>>()?;
    ensure(
        outcomes.iter().filter(|outcome| outcome.is_ok()).count() == 1
            && outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Err(crate::approval::Error::Conflict)))
                .count()
                == 1,
        "concurrent approval mutations both committed",
    )?;
    ensure(
        first
            .events(
                &expected.scope.workspace,
                Some(&expected.scope.session_id),
                0,
            )
            .map_err(unavailable)?
            .len()
            == 2,
        "concurrent mutation duplicated its event",
    )
}

pub fn schedule(
    store: &dyn crate::schedule::ScheduleStore,
    item: crate::schedule::store::NewItem,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let origin = item.origin.clone();
    let created = store.create(item, now).map_err(unavailable)?;
    let claimed = store
        .claim_deliveries(
            &crate::schedule::Scope::Origins(vec![origin.clone()]),
            std::time::Duration::from_secs(60),
            1,
            now,
        )
        .map_err(unavailable)?;
    ensure(
        claimed.len() == 1 && claimed[0].id == created.id,
        "scheduled occurrence claim differs",
    )?;
    ensure(
        store
            .claim_deliveries(
                &crate::schedule::Scope::Origins(vec![origin.clone()]),
                std::time::Duration::from_secs(60),
                1,
                now,
            )
            .map_err(unavailable)?
            .is_empty(),
        "scheduled occurrence was claimed twice",
    )?;
    ensure(
        store
            .delivered(created.id, created.due_at, now)
            .map_err(unavailable)?,
        "delivery receipt missing",
    )?;
    ensure(
        !store
            .delivered(created.id, created.due_at, now)
            .map_err(unavailable)?,
        "duplicate delivery repeated state change",
    )?;
    ensure(
        store
            .pending_writebacks(&origin, &created.session_id, 10)
            .map_err(unavailable)?
            .len()
            == 1,
        "delivery write-back receipt differs",
    )?;
    store
        .mark_appended(created.id, created.due_at)
        .map_err(unavailable)?;
    ensure(
        store
            .pending_writebacks(&origin, &created.session_id, 10)
            .map_err(unavailable)?
            .is_empty(),
        "appended delivery remained pending",
    )
}

pub fn workflow(
    store: &dyn crate::workflow::WorkflowStore,
    id: &str,
    definition: &crate::workflow::Definition,
) -> Result<()> {
    let run = store
        .create_run(
            id,
            definition,
            "/contract",
            "p",
            "openai-compatible",
            "m",
            "",
        )
        .map_err(unavailable)?;
    ensure(
        store.get_run(id).map_err(unavailable)? == Some(run),
        "workflow read differs",
    )?;
    let step = &definition.steps[0].id;
    let attempt = store.claim_ready(id, step, "").map_err(unavailable)?;
    ensure(
        attempt == 1 && store.claim_ready(id, step, "").is_err(),
        "workflow step claim was not exclusive",
    )?;
    store.recover("/contract").map_err(unavailable)?;
    let recovered = store
        .get_run(id)
        .map_err(unavailable)?
        .ok_or_else(|| Error::Invalid("workflow disappeared".into()))?;
    ensure(
        recovered.status == crate::workflow::RunStatus::Paused
            && recovered.steps[0].status == crate::workflow::StepStatus::Interrupted,
        "workflow recovery repeated an unknown effect",
    )
}

pub fn tasks(
    store: &dyn crate::subagent::record::TaskRepository,
    context: &crate::subagent::record::TaskContext,
    task: &crate::subagent::tasks::Task,
) -> Result<()> {
    store.upsert(context, task);
    store.upsert(context, task);
    let row = store
        .get(&context.parent_session, &task.id)
        .map_err(unavailable)?
        .ok_or_else(|| Error::Invalid("task snapshot missing".into()))?;
    ensure(
        row.task_id == task.id && row.status == task.status.as_str(),
        "task snapshot differs",
    )?;
    let query = crate::subagent::record::ListQuery {
        workspace: Some(context.workspace.clone()),
        ..Default::default()
    };
    ensure(
        store
            .list(&query)
            .map_err(unavailable)?
            .tasks
            .iter()
            .filter(|row| row.task_id == task.id && row.parent_session == context.parent_session)
            .count()
            == 1,
        "task upsert duplicated the snapshot",
    )
}

pub fn usage(
    store: &dyn crate::usage::repository::UsageRepository,
    record: &crate::usage::UsageRecord,
) -> Result<()> {
    let before = store
        .summary(Some(&record.context.session_id))
        .map_err(unavailable)?;
    store.append(record).map_err(unavailable)?;
    let after = store
        .summary(Some(&record.context.session_id))
        .map_err(unavailable)?;
    ensure(
        after.usage_events == before.usage_events + 1
            && after.reported_usage_events
                == before.reported_usage_events + i64::from(record.usage_present),
        "usage presence or event count differs",
    )?;
    ensure(
        after.input_tokens == before.input_tokens + record.usage.input_tokens,
        "usage aggregation differs",
    )
}

pub fn reflection(
    store: &dyn crate::reflection::repository::ReflectionStore,
    row: &crate::reflection::store::RunRow,
) -> Result<()> {
    store.record(row).map_err(unavailable)?;
    ensure(
        store
            .watermark(&row.session_id)
            .map_err(unavailable)?
            .as_deref()
            == Some(row.to_entry.as_str()),
        "reflection watermark differs",
    )
}

pub fn skill_checks(
    store: &dyn crate::skill::check_repository::CheckStore,
    row: &crate::skill::check_repository::NewRow<'_>,
) -> Result<()> {
    store.append(row).map_err(unavailable)?;
    let stored = store
        .latest(row.content_sha256, row.questions_sha256)
        .map_err(unavailable)?
        .ok_or_else(|| Error::Invalid("skill check audit missing".into()))?;
    ensure(
        &stored.result == row.result && stored.checked_at == row.checked_at,
        "skill check result changed",
    )
}

pub fn memory_advice(
    store: &dyn crate::memory::advice::audit::AuditStore,
    row: &crate::memory::advice::audit::NewRow,
) -> Result<()> {
    store.append(row, None).map_err(unavailable)
}

/// All record families share the staged command/receipt protocol.
pub async fn import_receipts(
    stage: &dyn StageImport,
    migration: &str,
    domain: &str,
    chunk: &Chunk,
    max_bytes: usize,
) -> Result<()> {
    let hash = chunk_hash(chunk, max_bytes)?;
    let first = stage.apply_chunk(migration, domain, chunk, &hash).await?;
    ensure(
        stage.apply_chunk(migration, domain, chunk, &hash).await? == first,
        "matching chunk command did not return its receipt",
    )?;
    ensure(
        stage
            .chunk_receipt(migration, domain, &chunk.cursor)
            .await?
            == Some(first),
        "committed chunk receipt is missing",
    )?;
    let mut changed = chunk.clone();
    changed.next_cursor = Some("changed-command-arguments".into());
    let hash = chunk_hash(&changed, max_bytes)?;
    ensure(
        stage
            .apply_chunk(migration, domain, &changed, &hash)
            .await
            .is_err(),
        "chunk command accepted different arguments",
    )
}
