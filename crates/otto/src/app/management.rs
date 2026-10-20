//! Shared command parsing and management operations used by frontends.
use std::sync::Arc;

use crate::app::Controller;
use crate::memory::{
    self, CandidateRef, ForgetRequest, ListRequest, RecordRef, RememberRequest, ReviewDecision,
    ReviewRequest, Scope, SearchRequest, SearchResult, Service,
};
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

#[derive(Debug, Clone)]
pub(crate) struct GeneratedSkillStatus {
    pub skill: crate::reflection::store::GeneratedSkill,
    pub still_owned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeSelector {
    Current,
    User,
    Workspace,
    All,
}

#[derive(Debug, Clone)]
pub(crate) enum Request {
    MemoryList {
        scope: ScopeSelector,
        limit: usize,
        cursor: String,
    },
    MemoryShow {
        id: String,
    },
    MemorySearch {
        query: String,
    },
    MemoryForget {
        id: String,
    },
    MemoryReviewList,
    MemoryReview {
        id: String,
        decision: ReviewDecision,
    },
    Remember {
        scope: String,
        kind: String,
        key: String,
        text: String,
    },
    Skills,
    SkillManager,
    SkillInfo(String),
    SkillDetail(String),
    SkillSet {
        name: String,
        enabled: bool,
    },
    SkillGenerated,
    SkillRevert(String),
    McpStatus,
    Timers,
    TimerCancel(String),
    Tasks,
    Task(String),
    TaskCancel(String),
    ReflectList,
    ReflectShow(String),
}

#[derive(Debug, Clone)]
pub(crate) enum Reply {
    MemoryList(memory::RecordPage),
    MemoryRecord(memory::Record),
    MemorySearch(SearchResult),
    MemoryCandidates(Vec<memory::Candidate>),
    MemoryForgotten {
        tombstone: memory::Tombstone,
        previous_revision: u64,
    },
    MemoryReviewed(ReviewDecision, Box<memory::ReviewResult>),
    Remembered(memory::Record),
    Skills(skill::Catalog),
    SkillManager(SkillManager),
    SkillInfo(SkillInfo),
    SkillDetail {
        skill: skill::Skill,
        body: String,
        contract_check: Option<String>,
        generated: Option<crate::reflection::store::GeneratedSkill>,
    },
    SkillSet(String),
    SkillGenerated(Vec<GeneratedSkillStatus>),
    SkillReverted {
        name: String,
        result: crate::reflection::skillwrite::Reverted,
    },
    McpStatus(Vec<crate::mcp::ServerStatus>),
    Timers(Vec<crate::tool::remind::StoredReminder>),
    TimerCancelled(crate::tool::remind::StoredReminder),
    Tasks(Vec<crate::app::Task>),
    Task(crate::app::Task),
    TaskCancelled(String),
    ReflectList(Vec<crate::reflection::runlog::Summary>),
    ReflectShow {
        id: String,
        log: Option<(std::path::PathBuf, String)>,
    },
    Notice(String),
    Usage(String),
}

impl Request {
    pub(crate) fn parse(command: &str, args: &str) -> Result<Self, String> {
        let fields: Vec<&str> = args.split_whitespace().collect();
        let usage = match command {
            "memory" => {
                "usage: /memory list [--scope current|user|workspace|all] [--limit N] [--cursor CURSOR] | /memory show <id> | /memory search <query> | /memory forget <id> | /memory review <id> accept|reject"
            }
            "remember" => "usage: /remember [--scope user|workspace] [--kind K] [--key K] <text>",
            "skill" => {
                "usage: /skill | /skill <name> | /skill set <name> enabled|disabled | /skill generated | /skill revert <name>"
            }
            "mcp" => "usage: /mcp",
            "timers" => "usage: /timers [cancel <id>]",
            "tasks" => "usage: /tasks",
            "task" => "usage: /task [<id|name> | cancel <id|name>]",
            "reflect" => "usage: /reflect list | /reflect show <id>",
            _ => return Err(format!("unknown management command: {command}")),
        };
        let invalid = || usage.to_string();
        Ok(match command {
            "memory" => match fields.as_slice() {
                ["list", rest @ ..] => {
                    let mut scope = ScopeSelector::Current;
                    let mut limit = 20;
                    let mut cursor = String::new();
                    let mut i = 0;
                    while i < rest.len() {
                        match rest[i] {
                            "--scope" if i + 1 < rest.len() => {
                                i += 1;
                                match rest[i] {
                                    "current" => scope = ScopeSelector::Current,
                                    "all" => scope = ScopeSelector::All,
                                    "user" => scope = ScopeSelector::User,
                                    "workspace" => scope = ScopeSelector::Workspace,
                                    _ => return Err(invalid()),
                                }
                            }
                            "--limit" if i + 1 < rest.len() => {
                                i += 1;
                                limit = rest[i].parse().map_err(|_| invalid())?;
                            }
                            "--cursor" if i + 1 < rest.len() => {
                                i += 1;
                                cursor = rest[i].to_owned();
                            }
                            _ => return Err(invalid()),
                        }
                        i += 1;
                    }
                    Request::MemoryList {
                        scope,
                        limit,
                        cursor,
                    }
                }
                ["show", id] => Request::MemoryShow { id: (*id).into() },
                ["search", rest @ ..] if !rest.is_empty() => Request::MemorySearch {
                    query: rest.join(" "),
                },
                ["forget", id] => Request::MemoryForget { id: (*id).into() },
                ["review"] => Request::MemoryReviewList,
                ["review", id, decision] => Request::MemoryReview {
                    id: (*id).into(),
                    decision: ReviewDecision::parse(decision).ok_or_else(invalid)?,
                },
                _ => return Err(invalid()),
            },
            "remember" => {
                let (mut scope, mut kind, mut key) =
                    (String::new(), "note".to_string(), String::new());
                let mut remaining = args;
                loop {
                    let trimmed = remaining.trim();
                    let Some((flag, rest)) = trimmed.split_once(char::is_whitespace) else {
                        remaining = trimmed;
                        break;
                    };
                    let Some(value_and_rest) = (match flag {
                        "--scope" | "--kind" | "--key" => Some(rest.trim_start()),
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
                ["generated"] => Request::SkillGenerated,
                ["revert", name] => Request::SkillRevert((*name).into()),
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
            "reflect" => match fields.as_slice() {
                ["list"] => Request::ReflectList,
                ["show", id] => Request::ReflectShow((*id).into()),
                _ => return Err(invalid()),
            },
            _ => unreachable!(),
        })
    }
}

impl Controller {
    pub(crate) fn memory_manager(&self) -> Option<(Arc<Service>, Scope, Scope)> {
        let wiring = self.memory_wiring();
        (self.dynamic_content() && wiring.usable && wiring.extension.is_none()).then(|| {
            (
                Arc::clone(&wiring.service),
                wiring.user_scope.clone(),
                self.workspace_memory_scope().clone(),
            )
        })
    }
    /// Exact current session destination; never redirects a producer after /new.
    pub(crate) fn notification_mailbox(&self) -> Option<Arc<dyn otto_core::mailbox::Mailbox>> {
        self.current_runner()
            .map(|runner| Arc::clone(runner.mailbox()))
    }

    pub(crate) fn subagent_tasks(&self) -> Option<Arc<crate::subagent::tasks::Tasks>> {
        self.current_runner()?.tasks.clone()
    }

    /// `/remember` against a selected memory service. The caller is a
    /// synchronous management thread, so the async client call is driven with
    /// `block_in_place` as `storage::worker::blocking` does.
    fn remember_with_service(
        &self,
        scope: &str,
        kind: String,
        key: String,
        text: String,
    ) -> Result<Reply, String> {
        use otto_core::memory::{Completion, Content, ItemState, Origin};
        let wiring = self.memory_wiring();
        let client = match &wiring.client {
            Some(client) if wiring.usable => Arc::clone(client),
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
        Ok(Reply::Notice(line))
    }

    pub(crate) fn management(&self, request: Request) -> Result<Reply, String> {
        use Request::*;
        if self.memory_wiring().extension.is_some() {
            if let Remember {
                scope,
                kind,
                key,
                text,
            } = request
            {
                return self.remember_with_service(&scope, kind, key, text);
            }
            if matches!(
                &request,
                MemoryList { .. }
                    | MemoryShow { .. }
                    | MemorySearch { .. }
                    | MemoryForget { .. }
                    | MemoryReviewList
                    | MemoryReview { .. }
            ) {
                return Err("memory operation is unsupported by the native record management interface; the selected memory service manages its records".into());
            }
        }
        match request {
            MemoryList {
                scope,
                limit,
                cursor,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let (all_scopes, scopes) = match scope {
                    ScopeSelector::All => (true, Vec::new()),
                    ScopeSelector::User => (false, vec![user]),
                    ScopeSelector::Workspace => (false, vec![workspace]),
                    ScopeSelector::Current => (false, vec![user, workspace]),
                };
                Ok(Reply::MemoryList(
                    service
                        .list(&ListRequest {
                            all_scopes,
                            scopes,
                            kinds: vec![],
                            labels: vec![],
                            limit,
                            cursor,
                            now: Utc::now(),
                            include_expired: true,
                        })
                        .map_err(|e| e.to_string())?,
                ))
            }
            MemoryShow { id } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                for scope in [user, workspace] {
                    match service.get(&RecordRef {
                        scope,
                        id: id.clone(),
                    }) {
                        Ok(r) => return Ok(Reply::MemoryRecord(r)),
                        Err(error) if error.kind == memory::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
                Err(format!("record {id} not found"))
            }
            MemorySearch { query } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                Ok(Reply::MemorySearch(
                    service
                        .search(&SearchRequest {
                            query,
                            scopes: vec![user, workspace],
                            limit: SEARCH_LIMIT,
                            token_budget: SEARCH_TOKEN_BUDGET,
                            now: Utc::now(),
                            ..Default::default()
                        })
                        .map_err(|e| e.to_string())?,
                ))
            }
            MemoryForget { id } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let mut last = None;
                for scope in [user, workspace] {
                    let reference = RecordRef {
                        scope,
                        id: id.clone(),
                    };
                    match service.get(&reference) {
                        Ok(r) => {
                            let revision = r.revision;
                            let tombstone = service
                                .forget(&ForgetRequest {
                                    reference,
                                    expected_revision: revision,
                                    purge_backups: false,
                                    confirm_purge: false,
                                })
                                .map_err(|e| e.to_string())?
                                .tombstone;
                            return Ok(Reply::MemoryForgotten {
                                tombstone,
                                previous_revision: revision,
                            });
                        }
                        Err(e) if e.kind == memory::ErrorKind::NotFound => last = Some(e),
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Err(format!(
                    "record {id} not found{}",
                    last.map(|e| format!(": {e}")).unwrap_or_default()
                ))
            }
            MemoryReviewList => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let pending = self
                    .approval_snapshot()
                    .map_err(|error| error.to_string())?;
                let candidates = crate::approval::memory::human_pending_candidates(
                    &service,
                    &[user, workspace],
                    &pending.requests,
                    SEARCH_LIMIT,
                )
                .map_err(|error| error.to_string())?;
                Ok(Reply::MemoryCandidates(candidates))
            }
            MemoryReview { id, decision } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let mut reference = None;
                for scope in [user, workspace] {
                    let candidate_ref = CandidateRef {
                        scope,
                        id: id.clone(),
                    };
                    match service.get_candidate(&candidate_ref) {
                        Ok(_) => {
                            reference = Some(candidate_ref);
                            break;
                        }
                        Err(error) if error.kind == memory::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
                let reference = reference.ok_or_else(|| format!("candidate {id} not found"))?;
                let result = self
                    .review_memory(
                        &ReviewRequest {
                            reference,
                            decision,
                            edited: None,
                            target_revision: None,
                        },
                        "memory-command",
                    )
                    .map_err(|e| e.to_string())?;
                Ok(Reply::MemoryReviewed(decision, Box::new(result)))
            }
            Remember {
                scope,
                kind,
                key,
                text,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let scope = match scope.as_str() {
                    "user" => user,
                    "workspace" => workspace,
                    _ => {
                        return Ok(Reply::Usage(format!(
                            "unknown scope {scope:?}, want user or workspace"
                        )));
                    }
                };
                Ok(Reply::Remembered(
                    service
                        .remember(&RememberRequest {
                            scope,
                            kind,
                            key,
                            text,
                            ..Default::default()
                        })
                        .map_err(|e| e.to_string())?,
                ))
            }
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
                let generated = self
                    .builder()
                    .reflector
                    .generated_skill(&name)
                    .filter(|row| {
                        self.reflection_skill_roots().is_some_and(|roots| {
                            crate::reflection::skillwrite::still_owned(&roots, row)
                        })
                    });
                Ok(Reply::SkillDetail {
                    skill: found.clone(),
                    body,
                    contract_check,
                    generated,
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
            SkillGenerated => {
                let roots = self.reflection_skill_roots();
                let rows = self
                    .builder()
                    .reflector
                    .generated_skills()
                    .map_err(|e| format!("skill generated: {e}"))?;
                Ok(Reply::SkillGenerated(
                    rows.into_iter()
                        .map(|skill| GeneratedSkillStatus {
                            still_owned: roots.as_ref().is_some_and(|roots| {
                                crate::reflection::skillwrite::still_owned(roots, &skill)
                            }),
                            skill,
                        })
                        .collect(),
                ))
            }
            SkillRevert(name) => {
                let roots = self.reflection_skill_roots().ok_or_else(|| {
                    "skill revert: skills are not available (check [skills].enabled)".to_string()
                })?;
                let result = self
                    .builder()
                    .reflector
                    .revert_skill(&roots, &name)
                    .map_err(|e| format!("skill revert: {e}"))?;
                Ok(Reply::SkillReverted { name, result })
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
            ReflectList => Ok(Reply::ReflectList(
                self.reflection_runs()
                    .map_err(|e| format!("/reflect list: {e}"))?,
            )),
            ReflectShow(id) => Ok(Reply::ReflectShow {
                id: id.clone(),
                log: self
                    .reflection_run(&id)
                    .map_err(|e| format!("/reflect show: {e}"))?,
            }),
        }
    }
}

#[cfg(test)]
mod tests;
