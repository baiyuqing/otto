//! Shared command parsing and management operations used by frontends.
use std::sync::Arc;

use crate::app::Controller;
use crate::memory::{self, RememberRequest, ReviewDecision, Scope, SearchResult, Service};
use crate::skill;
use chrono::Utc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillManager {
    /// Latest persisted global execution switch, independent of row preferences.
    pub global_enabled: bool,
    pub skills: Vec<ManagedSkill>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManagedSkill {
    pub skill: skill::Skill,
    pub enabled: bool,
    pub loaded: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillInfo {
    pub skill: skill::Skill,
    pub body: String,
    pub contract_check: Option<String>,
}

const SEARCH_LIMIT: usize = 20;
const SEARCH_TOKEN_BUDGET: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeSelector {
    Current,
    User,
    Workspace,
    All,
}

/// The `/memory list` filters beyond scope and paging. Empty `states` lists
/// active records only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListFilters {
    pub states: Vec<memory::RecordState>,
    pub kinds: Vec<String>,
    pub labels: Vec<String>,
    pub updated_after: Option<chrono::DateTime<Utc>>,
    pub verified_after: Option<chrono::DateTime<Utc>>,
    pub unverified_only: bool,
    pub review_due_only: bool,
}

/// A review deadline `AGE` (`30d`, `12h`, `45m`) counted forward from now.
pub fn review_deadline(age: &str) -> Option<chrono::DateTime<Utc>> {
    if chrono::DateTime::parse_from_rfc3339(age).is_ok() {
        // An absolute time is not an age, whether it is in the past or the future.
        return None;
    }
    let now = Utc::now();
    let past = parse_time_or_age(age, now)?;
    now.checked_add_signed(now - past)
}

/// An absolute RFC 3339 time, or an age such as `30d`, `12h` or `45m`
/// counted back from `now`.
pub fn parse_time_or_age(value: &str, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(time.with_timezone(&Utc));
    }
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    let count: i64 = digits.parse().ok()?;
    if count <= 0 {
        return None;
    }
    let age = match unit {
        "d" => chrono::Duration::days(count),
        "h" => chrono::Duration::hours(count),
        "m" => chrono::Duration::minutes(count),
        _ => return None,
    };
    now.checked_sub_signed(age)
}

#[derive(Debug, Clone)]
pub enum Request {
    MemoryList {
        scope: ScopeSelector,
        limit: usize,
        cursor: String,
        filters: ListFilters,
    },
    /// Moves a record to a lifecycle state; `superseded_by` is the
    /// replacement for `Superseded` and empty otherwise.
    MemoryState {
        id: String,
        state: memory::RecordState,
        superseded_by: String,
        reason: String,
        /// The revision the caller saw; `None` uses the current revision.
        expected_revision: Option<u64>,
    },
    /// Records a human verification with an optional next review deadline.
    MemoryVerify {
        id: String,
        review_at: Option<chrono::DateTime<Utc>>,
        expected_revision: Option<u64>,
    },
    /// Asks the proposal generator for the narrowest lifecycle change of a
    /// record that `evidence` supports; the result is a pending candidate.
    MemoryPropose {
        id: String,
        evidence: String,
    },
    /// Human-authorized replacement of evidence bindings, at an exact revision.
    MemoryBind {
        id: String,
        expected_revision: u64,
        evidence: Vec<memory::evidence::Evidence>,
        applicability: String,
    },
    MemoryShow {
        id: String,
    },
    MemorySearch {
        query: String,
    },
    MemoryForget {
        id: String,
        expected_revision: Option<u64>,
    },
    MemoryHistory {
        id: String,
    },
    /// Compares two revisions; `to` defaults to the current revision.
    MemoryDiff {
        id: String,
        from: u64,
        to: Option<u64>,
    },
    MemoryRestore {
        id: String,
        revision: u64,
        expected_revision: Option<u64>,
    },
    MemoryReviewList,
    /// One page of the candidates awaiting a human decision.
    MemoryCandidatePage {
        limit: usize,
        cursor: String,
    },
    MemoryReview {
        id: String,
        decision: ReviewDecision,
        /// The approval actor label of the frontend that decided.
        transport: &'static str,
    },
    Remember {
        scope: String,
        kind: String,
        key: String,
        text: String,
        review_at: Option<chrono::DateTime<Utc>>,
    },
    Skills,
    SkillManager,
    SkillInfo(String),
    SkillDetail(String),
    SkillSet {
        name: String,
        enabled: bool,
    },
    McpStatus,
    Timers,
    TimerCancel(String),
    Tasks,
    Task(String),
    TaskCancel(String),
}

#[derive(Debug, Clone)]
pub enum Reply {
    MemoryList(memory::RecordPage),
    MemoryRecord(memory::Record),
    MemorySearch(SearchResult),
    MemoryCandidates(Vec<memory::Candidate>),
    MemoryCandidatePage(memory::CandidatePage),
    MemoryForgotten {
        tombstone: memory::Tombstone,
        previous_revision: u64,
        cleanup: Vec<memory::ForgetCleanupEntry>,
    },
    MemoryHistory(memory::RecordHistory),
    MemoryDiff {
        from: Box<memory::Record>,
        to: Box<memory::Record>,
    },
    MemoryRestored(memory::Record),
    MemoryStateChanged(memory::Record),
    MemoryVerified(memory::Record),
    MemoryProposed(memory::proposals::Outcome),
    MemoryReviewed(ReviewDecision, Box<memory::ReviewResult>),
    Remembered(memory::Record),
    Skills(skill::Catalog),
    SkillManager(SkillManager),
    SkillInfo(SkillInfo),
    SkillDetail {
        skill: skill::Skill,
        body: String,
        contract_check: Option<String>,
    },
    SkillSet(String),
    McpStatus(Vec<crate::mcp::ServerStatus>),
    Timers(Vec<crate::schedule::remind::StoredReminder>),
    TimerCancelled(crate::schedule::remind::StoredReminder),
    Tasks(Vec<crate::app::Task>),
    Task(crate::app::Task),
    TaskCancelled(String),
    MemoryServiceResult {
        status: otto_core::session::local_command::LocalCommandStatus,
        text: String,
    },
    Notice(String),
    Usage(String),
}

impl Request {
    pub fn parse(command: &str, args: &str) -> Result<Self, String> {
        let fields: Vec<&str> = args.split_whitespace().collect();
        let usage = match command {
            "memory" => {
                "usage: /memory list [--scope current|user|workspace|all] [--limit N] \
                    [--cursor CURSOR] [--state active|needs-review|superseded|retracted|all] \
                    [--kind K] [--label L] [--updated-after TIME|AGE] \
                    [--verified-after TIME|AGE] [--unverified] [--review-due] | \
                    /memory show <id> | /memory search <query> | \
                    /memory forget <id> | /memory history <id> | \
                    /memory diff <id> <from> [<to>] | /memory restore <id> <revision> | \
                    /memory state <id> active|needs-review|retracted [--reason TEXT] | \
                    /memory state <id> superseded <replacement-id> [--reason TEXT] | \
                    /memory verify <id> [--review-in AGE] | \
                    /memory bind <id> <expected-revision> <compact-evidence-json> <applicability> | \
                    /memory condition-changed <id> <reason> | \
                    /memory propose <id> <evidence text> | \
                    /memory review <id> accept|reject"
            }
            "remember" => {
                "usage: /remember [--scope user|workspace] [--kind K] [--key K] \
                    [--review-in AGE] <text>"
            }
            "skill" => "usage: /skill | /skill <name> | /skill set <name> enabled|disabled",
            "mcp" => "usage: /mcp",
            "timers" => "usage: /timers [cancel <id>]",
            "tasks" => "usage: /tasks",
            "task" => "usage: /task [<id|name> | cancel <id|name>]",
            _ => return Err(format!("unknown management command: {command}")),
        };
        let invalid = || usage.to_string();
        Ok(match command {
            "memory" => match fields.as_slice() {
                ["list", rest @ ..] => {
                    let mut scope = ScopeSelector::Current;
                    let mut limit = 20;
                    let mut cursor = String::new();
                    let mut filters = ListFilters::default();
                    let now = Utc::now();
                    let mut i = 0;
                    while i < rest.len() {
                        let value = |i: usize| rest.get(i + 1).copied().ok_or_else(invalid);
                        match rest[i] {
                            "--scope" => {
                                scope = match value(i)? {
                                    "current" => ScopeSelector::Current,
                                    "all" => ScopeSelector::All,
                                    "user" => ScopeSelector::User,
                                    "workspace" => ScopeSelector::Workspace,
                                    _ => return Err(invalid()),
                                };
                                i += 1;
                            }
                            "--limit" => {
                                limit = value(i)?.parse().map_err(|_| invalid())?;
                                i += 1;
                            }
                            "--cursor" => {
                                cursor = value(i)?.to_owned();
                                i += 1;
                            }
                            "--state" => {
                                let state = value(i)?;
                                if state == "all" {
                                    filters.states = memory::RecordState::ALL.to_vec();
                                } else {
                                    filters.states.push(
                                        memory::RecordState::parse(state).ok_or_else(invalid)?,
                                    );
                                }
                                i += 1;
                            }
                            "--kind" => {
                                filters.kinds.push(value(i)?.to_owned());
                                i += 1;
                            }
                            "--label" => {
                                filters.labels.push(value(i)?.to_owned());
                                i += 1;
                            }
                            "--updated-after" => {
                                filters.updated_after =
                                    Some(parse_time_or_age(value(i)?, now).ok_or_else(invalid)?);
                                i += 1;
                            }
                            "--verified-after" => {
                                filters.verified_after =
                                    Some(parse_time_or_age(value(i)?, now).ok_or_else(invalid)?);
                                i += 1;
                            }
                            "--unverified" => filters.unverified_only = true,
                            "--review-due" => filters.review_due_only = true,
                            _ => return Err(invalid()),
                        }
                        i += 1;
                    }
                    Request::MemoryList {
                        scope,
                        limit,
                        cursor,
                        filters,
                    }
                }
                ["state", id, state, rest @ ..] => {
                    let state = memory::RecordState::parse(state).ok_or_else(invalid)?;
                    let mut rest = rest;
                    let mut superseded_by = String::new();
                    if state == memory::RecordState::Superseded {
                        let (replacement, tail) = rest.split_first().ok_or_else(invalid)?;
                        if replacement.starts_with("--") {
                            return Err(invalid());
                        }
                        superseded_by = (*replacement).to_owned();
                        rest = tail;
                    }
                    let reason = match rest {
                        [] => String::new(),
                        ["--reason", words @ ..] if !words.is_empty() => words.join(" "),
                        _ => return Err(invalid()),
                    };
                    Request::MemoryState {
                        id: (*id).into(),
                        state,
                        superseded_by,
                        reason,
                        expected_revision: None,
                    }
                }
                ["bind", id, revision, json, words @ ..] if !words.is_empty() => {
                    Request::MemoryBind {
                        id: (*id).into(),
                        expected_revision: revision.parse().map_err(|_| invalid())?,
                        evidence: serde_json::from_str(json).map_err(|_| invalid())?,
                        applicability: words.join(" "),
                    }
                }
                ["condition-changed", id, words @ ..] if !words.is_empty() => {
                    Request::MemoryState {
                        id: (*id).into(),
                        state: memory::RecordState::NeedsReview,
                        superseded_by: String::new(),
                        reason: words.join(" "),
                        expected_revision: None,
                    }
                }
                ["verify", id] => Request::MemoryVerify {
                    id: (*id).into(),
                    review_at: None,
                    expected_revision: None,
                },
                ["verify", id, "--review-in", age] => Request::MemoryVerify {
                    id: (*id).into(),
                    review_at: Some(review_deadline(age).ok_or_else(invalid)?),
                    expected_revision: None,
                },
                ["propose", id, words @ ..] if !words.is_empty() => Request::MemoryPropose {
                    id: (*id).into(),
                    evidence: words.join(" "),
                },
                ["show", id] => Request::MemoryShow { id: (*id).into() },
                ["search", rest @ ..] if !rest.is_empty() => Request::MemorySearch {
                    query: rest.join(" "),
                },
                ["forget", id] => Request::MemoryForget {
                    id: (*id).into(),
                    expected_revision: None,
                },
                ["history", id] => Request::MemoryHistory { id: (*id).into() },
                ["diff", id, from] => Request::MemoryDiff {
                    id: (*id).into(),
                    from: from.parse().map_err(|_| invalid())?,
                    to: None,
                },
                ["diff", id, from, to] => Request::MemoryDiff {
                    id: (*id).into(),
                    from: from.parse().map_err(|_| invalid())?,
                    to: Some(to.parse().map_err(|_| invalid())?),
                },
                ["restore", id, revision] => Request::MemoryRestore {
                    id: (*id).into(),
                    revision: revision.parse().map_err(|_| invalid())?,
                    expected_revision: None,
                },
                ["review"] => Request::MemoryReviewList,
                ["review", id, decision] => Request::MemoryReview {
                    id: (*id).into(),
                    decision: ReviewDecision::parse(decision).ok_or_else(invalid)?,
                    transport: "memory-command",
                },
                _ => return Err(invalid()),
            },
            "remember" => {
                let (mut scope, mut kind, mut key) =
                    (String::new(), "note".to_string(), String::new());
                let mut review_at = None;
                let mut remaining = args;
                loop {
                    let trimmed = remaining.trim();
                    let Some((flag, rest)) = trimmed.split_once(char::is_whitespace) else {
                        remaining = trimmed;
                        break;
                    };
                    let Some(value_and_rest) = (match flag {
                        "--scope" | "--kind" | "--key" | "--review-in" => Some(rest.trim_start()),
                        _ => None,
                    }) else {
                        remaining = trimmed;
                        break;
                    };
                    let Some((value, tail)) = value_and_rest.split_once(char::is_whitespace) else {
                        return Err(invalid());
                    };
                    match flag {
                        "--scope" => scope = value.to_string(),
                        "--kind" => kind = value.to_string(),
                        "--key" => key = value.to_string(),
                        "--review-in" => {
                            review_at = Some(review_deadline(value).ok_or_else(invalid)?)
                        }
                        _ => unreachable!(),
                    }
                    remaining = tail.trim_start();
                }
                let text = remaining.trim();
                if text.is_empty() {
                    return Err(invalid());
                }
                Request::Remember {
                    scope: if scope.is_empty() {
                        "workspace".into()
                    } else {
                        scope
                    },
                    kind,
                    key,
                    text: text.to_string(),
                    review_at,
                }
            }
            "skill" => match fields.as_slice() {
                [] => Request::Skills,
                ["set", name, state] => match *state {
                    "enabled" => Request::SkillSet {
                        name: (*name).into(),
                        enabled: true,
                    },
                    "disabled" => Request::SkillSet {
                        name: (*name).into(),
                        enabled: false,
                    },
                    _ => return Err(invalid()),
                },
                [name] => Request::SkillDetail((*name).into()),
                _ => return Err(invalid()),
            },
            "mcp" => {
                if fields.is_empty() {
                    Request::McpStatus
                } else {
                    return Err(invalid());
                }
            }
            "timers" => match fields.as_slice() {
                [] => Request::Timers,
                ["cancel", id] => Request::TimerCancel((*id).into()),
                _ => return Err(invalid()),
            },
            "tasks" => {
                if !fields.is_empty() {
                    return Err(invalid());
                }
                Request::Tasks
            }
            "task" => match fields.as_slice() {
                [] => Request::Tasks,
                ["cancel", id] => Request::TaskCancel((*id).into()),
                [id] if *id != "cancel" => Request::Task((*id).into()),
                _ => return Err(invalid()),
            },
            _ => unreachable!(),
        })
    }
}

impl Controller {
    pub fn memory_manager(&self) -> Option<(Arc<Service>, Scope, Scope)> {
        let wiring = self.memory_wiring();
        (self.dynamic_content() && wiring.usable && !wiring.external()).then(|| {
            (
                Arc::clone(&wiring.service),
                wiring.user_scope.clone(),
                self.workspace_memory_scope().clone(),
            )
        })
    }
    /// Exact current session destination; never redirects a producer after /new.
    pub fn notification_mailbox(&self) -> Option<Arc<dyn otto_core::mailbox::Mailbox>> {
        self.current_runner()
            .map(|runner| Arc::clone(runner.mailbox()))
    }

    pub fn subagent_tasks(&self) -> Option<Arc<crate::subagent::tasks::Tasks>> {
        self.current_runner()?.tasks().cloned()
    }

    /// `/remember` against a selected memory service. The caller is a
    /// synchronous management thread, so the async client call is driven with
    /// `block_in_place` as `otto_storage::worker::blocking` does.
    pub(super) fn remember_with_service(
        &self,
        scope: &str,
        kind: String,
        key: String,
        text: String,
    ) -> Result<Reply, String> {
        use otto_core::memory::{Completion, Content, ItemState, Origin};
        let wiring = self.memory_wiring();
        let runner_client = self
            .current_runner()
            .and_then(|runner| runner.memory_client.clone());
        let client = match runner_client.or_else(|| wiring.client.clone()) {
            Some(client) if wiring.usable => client,
            _ => return Err("memory is not available".into()),
        };
        let mut context = self.memory_context(Origin::Human);
        match scope {
            "workspace" => {}
            "user" => {
                context.write_scope = context
                    .scopes
                    .iter()
                    .find(|scope| scope.namespace == "user")
                    .cloned()
            }
            "organization" if context.organization_id.is_some() => {
                context.write_scope = context
                    .scopes
                    .iter()
                    .find(|scope| scope.namespace == "organization")
                    .cloned()
            }
            _ => {
                return Ok(Reply::Usage(format!(
                    "unknown scope {scope:?}, want user or workspace (organization requires a tenant-aware host)"
                )));
            }
        }
        let mut metadata = std::collections::BTreeMap::new();
        let authoritative_key = (!key.is_empty()).then(|| key.clone());
        for (name, value) in [("kind", kind), ("key", key)] {
            if !value.is_empty() {
                metadata.insert(name.to_string(), value);
            }
        }
        if context.organization_id.is_some() {
            metadata.remove("key");
        }
        let request = otto_core::memory::RememberRequest {
            key: authoritative_key,
            context,
            request_id: crate::memory::new_id().map_err(|e| e.to_string())?,
            content: Content::Text(text),
            target: None,
            metadata,
        };
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "memory service requires an async runtime".to_string())?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return Err("memory service requires a multi-thread runtime".into());
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let receipt = tokio::task::block_in_place(|| {
            if scope == "organization" {
                handle.block_on(client.remember_organization_human(&request, &cancel))
            } else {
                handle.block_on(client.remember(&request, &cancel))
            }
        })
        .map_err(|e| e.to_string())?;
        let pending = matches!(
            receipt.completion,
            Completion::Pending | Completion::Running
        );
        let mut line = match receipt.items.first() {
            Some(item) => {
                let state = match item.state {
                    ItemState::Applied => "applied",
                    ItemState::NoChange => "no_change",
                    ItemState::Skipped => "skipped",
                    ItemState::Failed => "failed",
                    ItemState::Unknown => "unknown",
                };
                match &item.reference {
                    Some(r) => format!(
                        "remembered {} (scope={}/{}, state={state})",
                        r.id, r.scope.namespace, r.scope.id
                    ),
                    None => format!("remember request {} (state={state})", receipt.request_id),
                }
            }
            None => format!(
                "remember request {} accepted (completion={:?})",
                receipt.request_id, receipt.completion
            ),
        };
        if pending {
            line.push_str(" pending");
        }
        use otto_core::session::local_command::LocalCommandStatus;
        let status = if pending {
            LocalCommandStatus::Pending
        } else if receipt.completion == Completion::Failed
            || receipt
                .items
                .iter()
                .any(|item| item.state == ItemState::Failed)
        {
            LocalCommandStatus::Error
        } else if receipt.completion == Completion::Unknown
            || receipt.items.is_empty()
            || receipt
                .items
                .iter()
                .any(|item| item.state == ItemState::Unknown)
        {
            LocalCommandStatus::Pending
        } else {
            LocalCommandStatus::Success
        };
        Ok(Reply::MemoryServiceResult { status, text: line })
    }

    pub fn management(&self, request: Request) -> Result<Reply, String> {
        self.management_typed(request)
            .map_err(|error| error.message)
    }

    /// The skill, MCP, timer and task requests, whose failures are text.
    pub(super) fn other_request(&self, request: Request) -> Result<Reply, String> {
        use Request::*;
        match request {
            Skills => Ok(Reply::Skills(self.skills())),
            SkillManager => self.managed_skills().map(Reply::SkillManager),
            SkillInfo(name) => {
                let catalog = self.discovered_skills();
                let found = catalog
                    .lookup(&name)
                    .ok_or_else(|| format!("unknown skill: {name}"))?;
                let body = skill::load(found).map_err(|e| format!("skill {name}: {e}"))?;
                let contract_check = found.contract.as_ref().and_then(|_| {
                    self.skill_checker()
                        .map(|c| format!("Contract check: {}", c.display(found)))
                });
                Ok(Reply::SkillInfo(crate::app::management::SkillInfo {
                    skill: found.clone(),
                    body,
                    contract_check,
                }))
            }
            SkillDetail(name) => {
                let catalog = self.skills();
                let found = catalog
                    .lookup(&name)
                    .ok_or_else(|| format!("unknown skill: {name}"))?;
                let body = skill::load(found).map_err(|e| format!("skill {name}: {e}"))?;
                let contract_check = found.contract.as_ref().and_then(|_| {
                    self.skill_checker()
                        .map(|c| format!("Contract check: {}", c.display(found)))
                });
                Ok(Reply::SkillDetail {
                    skill: found.clone(),
                    body,
                    contract_check,
                })
            }
            SkillSet { name, enabled } => {
                crate::config::set_skill_disabled_file(
                    &self.builder().config_path,
                    &name,
                    !enabled,
                )
                .map_err(|e| format!("skill {name}: {e}"))?;
                Ok(Reply::SkillSet(format!(
                    "skill {name} preference is {}; restart Otto to apply the active catalog (activation also requires [skills].enabled = true)",
                    if enabled { "enabled" } else { "disabled" }
                )))
            }
            Tasks => Ok(Reply::Tasks(
                self.tasks().map(|t| t.list()).unwrap_or_default(),
            )),
            Task(id) => self
                .tasks()
                .and_then(|t| t.get(&id))
                .map(Reply::Task)
                .ok_or_else(|| "task not found".into()),
            TaskCancel(id) => {
                self.tasks()
                    .ok_or_else(|| "task not found".to_string())?
                    .cancel(&id)?;
                Ok(Reply::TaskCancelled(id))
            }
            McpStatus => Ok(Reply::McpStatus(self.mcp())),
            Timers => {
                let reminders = self
                    .reminders()
                    .ok_or_else(|| "timers are not available".to_string())?;
                Ok(Reply::Timers(reminders.list()))
            }
            TimerCancel(id) => {
                let reminders = self
                    .reminders()
                    .ok_or_else(|| "timers are not available".to_string())?;
                Ok(Reply::TimerCancelled(reminders.cancel(&id)?))
            }
            _ => unreachable!("memory requests are handled by management_typed"),
        }
    }
}

mod management_dispatch;
mod management_error;
mod memory_requests;
mod reply_text;

pub use management_error::{ManagementError, ManagementErrorKind};
pub use reply_text::{format_management_reply, format_mcp_report};

#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod proposal_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod typed_management_tests;
