//! Run-log items, existing-memory context and the propose request.

use super::*;

pub(super) fn memory_item(proposal: &Proposal) -> runlog::Item {
    let action = match proposal.action {
        Action::Create => "create",
        Action::Update => "update",
        Action::Forget => "forget",
    };
    let title = if proposal.target_id.is_empty() {
        format!(
            "{action} {} {} `{}`",
            proposal.scope.as_str(),
            proposal.kind,
            proposal.key
        )
    } else {
        format!(
            "{action} {} {} `{}` (target {})",
            proposal.scope.as_str(),
            proposal.kind,
            proposal.key,
            proposal.target_id
        )
    };
    runlog::Item {
        kind: runlog::Kind::Memory,
        title,
        text: proposal.text.clone(),
        body: String::new(),
        reason: proposal.reason.clone(),
        cited: proposal.cited.clone(),
        outcome: runlog::Outcome::Dropped(""),
    }
}

pub(super) fn skill_item(candidate: &guard::Candidate) -> runlog::Item {
    let action = match candidate.action {
        output::SkillAction::Create => "create",
        output::SkillAction::Revise => "revise",
    };
    runlog::Item {
        kind: runlog::Kind::Skill,
        title: format!("{action} skill `{}`", candidate.name),
        text: candidate.description.clone(),
        body: guard::content(&candidate.body, &candidate.scripts),
        reason: candidate.reason.clone(),
        cited: candidate.cited.clone(),
        outcome: runlog::Outcome::Dropped(""),
    }
}

/// The skill roots for a run: write to `~/.otto/skills`, keep history in
/// `~/.otto/skill-history`, and treat every configured root as taken. `None`
/// without a home directory.
pub fn skill_roots(home: &str, configured: &[String]) -> Option<skillwrite::Roots> {
    if home.is_empty() {
        return None;
    }
    let base = Path::new(home).join(".otto");
    Some(skillwrite::Roots {
        write_root: base.join("skills"),
        history_root: base.join("skill-history"),
        lookup_roots: configured.iter().map(std::path::PathBuf::from).collect(),
    })
}

pub(super) fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(super) fn bounded_focus(focus: &str) -> String {
    let cleaned: String = focus
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut cleaned = cleaned.trim().to_owned();
    while cleaned.len() > prompt::MAXIMUM_FOCUS_BYTES {
        cleaned.pop();
    }
    cleaned
}

pub(super) fn existing_memories(context: &Context<'_>) -> Result<Vec<Existing>, Error> {
    let mut existing = Vec::new();
    for (choice, scope) in [
        (ScopeChoice::User, context.user_scope),
        (ScopeChoice::Workspace, context.workspace_scope),
    ] {
        let page = context
            .service
            .list(&crate::memory::ListRequest {
                all_scopes: false,
                scopes: vec![scope.clone()],
                kinds: Vec::new(),
                labels: Vec::new(),
                limit: CONTEXT_LIMIT,
                cursor: String::new(),
                now: Utc::now(),
                include_expired: false,
            })
            .map_err(|_| Error::MemoryUnavailable)?;
        existing.extend(page.records.into_iter().map(|record| Existing {
            id: record.id,
            scope: choice,
            kind: record.kind,
            key: record.key,
            text: record.text,
            revision: record.revision,
        }));
    }
    Ok(existing)
}

/// A pending candidate, for duplicate suppression. The store clears a
/// rejected candidate's content, so a rejected proposal cannot be recognized
/// and may be proposed again.
pub(super) struct Known {
    scope: Scope,
    action: CandidateAction,
    kind: String,
    key: String,
    text: String,
    target_id: String,
}

impl Known {
    pub(super) fn matches(&self, scope: &Scope, proposal: &Proposal) -> bool {
        let action = match proposal.action {
            Action::Create => CandidateAction::Create,
            Action::Update => CandidateAction::Update,
            Action::Forget => CandidateAction::Forget,
        };
        self.scope == *scope
            && self.action == action
            && self.kind == proposal.kind
            && self.key == proposal.key
            && self.text == proposal.text
            && self.target_id == proposal.target_id
    }
}

/// Best effort: a failure to read candidates means no suppression, never a
/// failed run.
pub(super) fn known_candidates(context: &Context<'_>) -> Vec<Known> {
    let result = context.service.search(&SearchRequest {
        scopes: vec![context.user_scope.clone(), context.workspace_scope.clone()],
        include_candidates: true,
        candidate_states: vec![CandidateState::Pending],
        limit: CONTEXT_LIMIT,
        token_budget: 1,
        now: Utc::now(),
        ..SearchRequest::default()
    });
    result
        .map(|result| {
            result
                .candidates
                .into_iter()
                .map(|candidate| Known {
                    scope: candidate.proposed.scope,
                    action: candidate.action,
                    kind: candidate.proposed.kind,
                    key: candidate.proposed.key,
                    text: candidate.proposed.text,
                    target_id: candidate.target_id,
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn propose_request(
    context: &Context<'_>,
    run_id: &str,
    scope: Scope,
    proposal: &Proposal,
) -> ProposeRequest {
    ProposeRequest {
        action: match proposal.action {
            Action::Create => CandidateAction::Create,
            Action::Update => CandidateAction::Update,
            Action::Forget => CandidateAction::Forget,
        },
        scope,
        kind: proposal.kind.clone(),
        key: proposal.key.clone(),
        text: proposal.text.clone(),
        confidence: proposal.confidence,
        target_id: proposal.target_id.clone(),
        base_revision: proposal.base_revision,
        reason: proposal.reason.clone(),
        source: Provenance {
            origin: Some(Origin::Extractor),
            session_id: context.session_id.to_owned(),
            message_ids: proposal.cited.clone(),
            ..Provenance::default()
        },
        // The store refuses an observation id on a candidate, so the run id
        // travels as metadata for tracing a candidate back to its run.
        metadata: BTreeMap::from_iter(
            [(RUN_METADATA_KEY.to_owned(), run_id.to_owned())]
                .into_iter()
                .chain(context.presentation_origin.map(|origin| {
                    (
                        crate::memory::PRESENTATION_ORIGIN_METADATA.to_owned(),
                        origin.as_str().to_owned(),
                    )
                })),
        ),
        ..ProposeRequest::default()
    }
}
