//! Storage (SQL) rows of the fault-injection matrix: one `SqlFault` per
//! repository, injected through `FaultySqlDriver` under `SqlRepositories`.
//!
//! Each test covers one row and iterates the repositories; a failure names
//! the repository. Persisted state is read back through a plain
//! `Connection::open`, which uses the real driver.

use std::path::Path;
use std::sync::Arc;

use otto::runtime::storage::SqlRepositories;
use otto_faults::sql::{FaultySqlDriver, SqlOp};
use otto_faults::{Fault, FaultError, FaultPlan, Filter, Rule, SqlFault, Trigger};
use otto_storage::Connection;
use otto_storage::drivers::sqlite::SqliteDriver;

/// What a repository's caller learns from one write. Most repositories
/// collapse every storage error into one opaque domain error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Written,
    Unknown,
    /// A typed domain error that is neither busy nor unknown.
    Unavailable,
    /// The domain error carries no category.
    Opaque,
}

type Write = Box<dyn Fn(u32) -> Outcome>;
type List = Box<dyn Fn() -> Option<usize>>;

struct Handle {
    write: Write,
    /// Rows from the repository's multi-row read; `None` is an error.
    list: Option<List>,
}

struct Repo {
    name: &'static str,
    open: fn(&SqlRepositories, &str) -> Option<Handle>,
    /// Table holding one row per successful `write`.
    table: &'static str,
    /// Where one write commits.
    commit: (SqlOp, Filter),
    /// Calls matching `commit` that opening the repository makes. A rule
    /// that targets the first write fires on call `setup + 1`.
    setup: u32,
    /// Whether the write commits an explicit transaction, so a commit
    /// fault and a rollback are reachable; the others write in autocommit.
    transactional: bool,
    /// What the caller sees for `SqlFault::Busy` on `commit`.
    busy: Outcome,
    /// What the caller sees for a lost commit acknowledgement.
    unknown: Outcome,
    /// SQL prefix of the statement behind `Handle::list`.
    list_sql: &'static str,
}

const INSERT: (SqlOp, Filter) = (SqlOp::Execute, Filter::SqlPrefix("INSERT"));
const TX: (SqlOp, Filter) = (SqlOp::Commit, Filter::Any);

fn plan(rules: Vec<Rule<SqlOp>>) -> Arc<FaultPlan<SqlOp>> {
    FaultPlan::new(rules)
}

fn factory(plan: &Arc<FaultPlan<SqlOp>>) -> SqlRepositories {
    SqlRepositories::new(Arc::new(FaultySqlDriver::new(
        Arc::new(SqliteDriver),
        plan.clone(),
    )))
}

fn rule(op: SqlOp, filter: Filter, trigger: Trigger, fault: Fault) -> Rule<SqlOp> {
    Rule::new(op, trigger, fault).filter(filter)
}

fn sql(fault: SqlFault) -> FaultError {
    FaultError::Sql(fault)
}

fn rows(path: &Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("reopen with the real driver");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), (), |row| {
            row.get(0)
        })
        .expect("count rows")
}

// Fixtures: one write per repository. `n` makes each write a distinct row.

fn approvals(factory: &SqlRepositories, address: &str) -> Option<Handle> {
    use otto::approval::{self, Proposal};
    use otto_core::approval::{Authority, Kind, Scope, Subject};
    let store = factory.approvals(Path::new(address)).ok()?;
    let service = approval::Service::in_memory().unwrap();
    let scope = Scope {
        workspace: "/contract".into(),
        session_id: "memory:contract".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    service
        .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
        .unwrap();
    let listed = store.clone();
    Some(Handle {
        write: Box::new(move |n| {
            let request = service
                .create(Proposal {
                    kind: Kind::Confirmation,
                    authority: Authority::Consent,
                    scope: scope.clone(),
                    subject: Subject {
                        tool_call_id: String::new(),
                        source: "conversation".into(),
                        id: format!("operation-{n}"),
                        version: "1".into(),
                        details: "contract".into(),
                    },
                    title: "Review".into(),
                    reason: "test".into(),
                    persistent: false,
                    blocking: true,
                    expires_at: None,
                })
                .unwrap();
            match store.create(&format!("key-{n}"), &request) {
                Ok(_) => Outcome::Written,
                Err(approval::Error::CommitUnknown) => Outcome::Unknown,
                Err(_) => Outcome::Unavailable,
            }
        }),
        list: Some(Box::new(move || {
            listed.pending().ok().map(|rows| rows.len())
        })),
    })
}

fn schedule(factory: &SqlRepositories, address: &str) -> Option<Handle> {
    use otto::schedule::{Kind, store::NewItem};
    use otto_session::origin::Origin;
    let store = factory.schedule(Path::new(address)).ok()?;
    let listed = store.clone();
    let now = chrono::Utc::now();
    Some(Handle {
        write: Box::new(move |n| {
            let item = NewItem {
                origin: Origin::terminal(),
                session_id: "memory:contract".into(),
                workspace: "/contract".into(),
                kind: Kind::Text,
                body: format!("contract-{n}"),
                repeat: None,
                local_time: None,
                due_at: now,
            };
            match store.create(item, now) {
                Ok(_) => Outcome::Written,
                Err(_) => Outcome::Opaque,
            }
        }),
        list: Some(Box::new(move || {
            listed
                .list(&Origin::terminal(), now)
                .ok()
                .map(|items| items.len())
        })),
    })
}

fn workflow(factory: &SqlRepositories, address: &str) -> Option<Handle> {
    let store = factory.workflow(Path::new(address)).ok()?;
    let definition = otto::workflow::Definition {
        source: String::new(),
        name: "contract".into(),
        description: String::new(),
        steps: vec![otto::workflow::Step {
            id: "one".into(),
            kind: otto::workflow::StepKind::Agent,
            agent: "worker".into(),
            prompt: "contract".into(),
            needs: Vec::new(),
        }],
        agents: Vec::new(),
        hash: "hash".into(),
    };
    Some(Handle {
        write: Box::new(move |n| {
            match store.create_run(
                &format!("run-{n}"),
                &definition,
                "/contract",
                "p",
                "openai-compatible",
                "m",
                "",
            ) {
                Ok(_) => Outcome::Written,
                Err(_) => Outcome::Opaque,
            }
        }),
        list: None,
    })
}

fn tasks(factory: &SqlRepositories, address: &str) -> Option<Handle> {
    let store = factory.tasks(Path::new(address)).ok()?;
    let listed = store.clone();
    let context = otto::subagent::record::TaskContext {
        parent_session: "memory:contract".into(),
        workspace: "/contract".into(),
        ..Default::default()
    };
    Some(Handle {
        write: Box::new(move |n| {
            let now = chrono::Utc::now();
            // `Recorder::upsert` returns nothing: a failed write is not
            // visible to the caller.
            store.upsert(
                &context,
                &otto::subagent::tasks::Task {
                    id: format!("task-{n}"),
                    status: otto::subagent::tasks::TaskStatus::Succeeded,
                    created_at: Some(now),
                    finished_at: Some(now),
                    ..Default::default()
                },
            );
            Outcome::Written
        }),
        list: Some(Box::new(move || {
            let query = otto::subagent::record::ListQuery {
                workspace: Some("/contract".into()),
                ..Default::default()
            };
            listed.list(&query).ok().map(|result| result.tasks.len())
        })),
    })
}

fn usage(factory: &SqlRepositories, address: &str) -> Option<Handle> {
    let store = factory.usage(Path::new(address)).ok()?;
    Some(Handle {
        write: Box::new(move |_| {
            let record = otto::usage::UsageRecord {
                occurred_at: chrono::Utc::now().to_rfc3339(),
                context: otto::usage::Context {
                    session_id: "memory:contract".into(),
                    ..Default::default()
                },
                kind: "provider",
                usage: Default::default(),
                usage_present: true,
                cached_input_tokens_present: true,
            };
            match store.append(&record) {
                Ok(()) => Outcome::Written,
                Err(_) => Outcome::Opaque,
            }
        }),
        list: None,
    })
}

fn skill_checks(factory: &SqlRepositories, address: &str) -> Option<Handle> {
    use otto::skill::check_repository::{NewRow, ResultJson};
    let store = factory.skill_checks(Path::new(address)).ok()?;
    Some(Handle {
        write: Box::new(move |n| {
            let result = ResultJson::Rules { rules: Vec::new() };
            let row = NewRow {
                content_sha256: &format!("{n:064}"),
                questions_sha256: &"b".repeat(64),
                skill: "contract",
                path: "/contract",
                source: "rules",
                model: "m",
                checked_at: &chrono::Utc::now().to_rfc3339(),
                result: &result,
            };
            match store.append(&row) {
                Ok(()) => Outcome::Written,
                Err(_) => Outcome::Opaque,
            }
        }),
        list: None,
    })
}

fn memory_advice(factory: &SqlRepositories, address: &str) -> Option<Handle> {
    use otto::memory::advice::audit::NewRow;
    let store = factory.memory_advice(Path::new(address)).ok()?;
    Some(Handle {
        write: Box::new(move |_| {
            let row = NewRow {
                model: "m".into(),
                auxiliary: false,
                scope: "workspace".into(),
                request: "{}".into(),
                response: None,
                outcome: "ok",
                error: None,
                latency_ms: 0,
            };
            match store.append(&row, None) {
                Ok(()) => Outcome::Written,
                Err(_) => Outcome::Opaque,
            }
        }),
        list: None,
    })
}

fn repos() -> Vec<Repo> {
    use Outcome::{Opaque, Unavailable, Unknown, Written};
    #[allow(clippy::too_many_arguments)]
    fn repo(
        name: &'static str,
        open: fn(&SqlRepositories, &str) -> Option<Handle>,
        table: &'static str,
        commit: (SqlOp, Filter),
        setup: u32,
        busy: Outcome,
        unknown: Outcome,
        list_sql: &'static str,
    ) -> Repo {
        Repo {
            name,
            open,
            table,
            commit,
            setup,
            transactional: commit.0 != SqlOp::Execute,
            busy,
            unknown,
            list_sql,
        }
    }
    let approvals_sql = "SELECT body FROM approvals WHERE state";
    vec![
        repo(
            "approvals",
            approvals,
            "approvals",
            TX,
            1,
            Unavailable,
            Unknown,
            approvals_sql,
        ),
        repo(
            "schedule",
            schedule,
            "items",
            TX,
            1,
            Opaque,
            Opaque,
            "SELECT id, origin, session_id",
        ),
        repo(
            "workflow",
            workflow,
            "workflow_runs",
            TX,
            0,
            Opaque,
            Opaque,
            "",
        ),
        repo(
            "tasks",
            tasks,
            "tasks",
            (SqlOp::Execute, Filter::SqlPrefix("INSERT INTO tasks")),
            0,
            Written,
            Written,
            "SELECT parent_session, task_id",
        ),
        repo(
            "usage",
            usage,
            "usage_events",
            INSERT,
            0,
            Opaque,
            Opaque,
            "",
        ),
        repo(
            "skill_checks",
            skill_checks,
            "skill_checks",
            INSERT,
            0,
            Opaque,
            Opaque,
            "",
        ),
        repo(
            "memory_advice",
            memory_advice,
            "memory_advice",
            INSERT,
            0,
            Opaque,
            Opaque,
            "",
        ),
    ]
}

fn open(repo: &Repo, plan: &Arc<FaultPlan<SqlOp>>, path: &Path) -> Handle {
    (repo.open)(&factory(plan), path.to_str().unwrap())
        .unwrap_or_else(|| panic!("{}: open failed", repo.name))
}

#[test]
fn busy_commit_surfaces_typed_and_leaves_the_connection_usable() {
    for repo in repos() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let plan = plan(vec![rule(
            repo.commit.0,
            repo.commit.1,
            Trigger::Nth(repo.setup + 1),
            Fault::Before(sql(SqlFault::Busy)),
        )]);
        let handle = open(&repo, &plan, &path);
        let failed = (handle.write)(1);
        assert_eq!(failed, repo.busy, "{}: busy outcome", repo.name);
        assert_eq!(rows(&path, repo.table), 0, "{}: failed write", repo.name);
        assert_eq!(
            (handle.write)(2),
            Outcome::Written,
            "{}: next write",
            repo.name
        );
        // A busy write does not disable the task recorder: another process
        // holding the write lock is expected, and the next write succeeds.
        assert_eq!(rows(&path, repo.table), 1, "{}: after retry", repo.name);
        plan.assert_all_fired();
    }
}

/// The task recorder disables itself after the first write error other than
/// busy, so later writes are skipped without an error.
fn stops_after_error(repo: &Repo) -> bool {
    repo.name == "tasks"
}

fn transactional() -> Vec<Repo> {
    repos()
        .into_iter()
        .filter(|repo| repo.transactional)
        .collect()
}

#[test]
fn lost_commit_acknowledgement_reports_unknown_and_poisons_the_connection() {
    for repo in transactional() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let plan = plan(vec![rule(
            repo.commit.0,
            repo.commit.1,
            Trigger::Nth(repo.setup + 1),
            Fault::After(sql(SqlFault::CommitUnknown)),
        )]);
        let handle = open(&repo, &plan, &path);
        assert_eq!(
            (handle.write)(1),
            repo.unknown,
            "{}: unknown outcome",
            repo.name
        );
        assert_ne!(
            (handle.write)(2),
            Outcome::Written,
            "{}: poisoned connection accepted a write",
            repo.name
        );
        drop(handle);
        assert_eq!(
            rows(&path, repo.table),
            1,
            "{}: the acknowledged-lost commit is durable and the refused write is absent",
            repo.name
        );
        plan.assert_all_fired();
    }
}

#[test]
fn failed_rollback_poisons_the_connection_and_nothing_is_persisted() {
    for repo in transactional() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let plan = plan(vec![
            rule(
                repo.commit.0,
                repo.commit.1,
                Trigger::Nth(repo.setup + 1),
                Fault::Before(sql(SqlFault::Busy)),
            ),
            rule(
                SqlOp::Rollback,
                Filter::Any,
                Trigger::Nth(1),
                Fault::Before(sql(SqlFault::Unavailable)),
            ),
        ]);
        let handle = open(&repo, &plan, &path);
        assert_ne!(
            (handle.write)(1),
            Outcome::Written,
            "{}: busy write",
            repo.name
        );
        assert_ne!(
            (handle.write)(2),
            Outcome::Written,
            "{}: poisoned connection accepted a write",
            repo.name
        );
        drop(handle);
        assert_eq!(rows(&path, repo.table), 0, "{}: persisted rows", repo.name);
        plan.assert_all_fired();
    }
}

#[test]
fn lost_acknowledgement_of_an_autocommit_write_is_reported_and_the_row_is_stored() {
    for repo in repos().into_iter().filter(|repo| !repo.transactional) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let plan = plan(vec![rule(
            repo.commit.0,
            repo.commit.1,
            Trigger::Nth(repo.setup + 1),
            Fault::After(sql(SqlFault::Unavailable)),
        )]);
        let handle = open(&repo, &plan, &path);
        assert_eq!(
            (handle.write)(1),
            repo.busy,
            "{}: lost acknowledgement",
            repo.name
        );
        assert_eq!(
            (handle.write)(2),
            Outcome::Written,
            "{}: next write",
            repo.name
        );
        drop(handle);
        assert_eq!(
            rows(&path, repo.table),
            if stops_after_error(&repo) { 1 } else { 2 },
            "{}: persisted rows",
            repo.name
        );
        plan.assert_all_fired();
    }
}

#[test]
fn failed_open_returns_the_domain_error_and_creates_no_database() {
    for repo in repos() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let plan = plan(vec![rule(
            SqlOp::Open,
            Filter::NameSuffix("state.db"),
            Trigger::Nth(1),
            Fault::Before(sql(SqlFault::Unavailable)),
        )]);
        assert!(
            (repo.open)(&factory(&plan), path.to_str().unwrap()).is_none(),
            "{}: open succeeded",
            repo.name
        );
        assert!(!path.exists(), "{}: created a database", repo.name);
        plan.assert_all_fired();
    }
}

#[test]
fn step_error_in_the_middle_of_a_read_is_an_error_not_a_short_result() {
    for repo in repos().into_iter().filter(|repo| !repo.list_sql.is_empty()) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let clean = plan(Vec::new());
        let writer = open(&repo, &clean, &path);
        for n in 1..=3 {
            assert_eq!((writer.write)(n), Outcome::Written, "{}: seed", repo.name);
        }
        let list = writer.list.as_ref().expect("list");
        assert_eq!(list(), Some(3), "{}: unfaulted read", repo.name);
        drop(writer);

        let plan = plan(vec![rule(
            SqlOp::Step,
            Filter::SqlPrefix(repo.list_sql),
            Trigger::Nth(2),
            Fault::Before(sql(SqlFault::Unavailable)),
        )]);
        let reader = open(&repo, &plan, &path);
        let list = reader.list.as_ref().expect("list");
        assert_eq!(
            list(),
            None,
            "{}: read stopped at the failed step",
            repo.name
        );
        assert_eq!(list(), Some(3), "{}: read after the fault", repo.name);
        plan.assert_all_fired();
    }
}
