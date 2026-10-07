//! One review coordinator for tool and domain producers. The provider never
//! owns journal mutation or operation effects.
use super::{
    repository::ReviewLease,
    reviewer::{ReviewContext, ReviewProvider},
    *,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

type SessionKey = (String, String);
#[derive(Default)]
pub(super) struct ReviewState {
    providers: BTreeMap<SessionKey, Arc<dyn ReviewProvider>>,
    contexts: BTreeMap<SessionKey, ReviewContext>,
    configs: BTreeMap<String, otto_core::config::ApprovalConfig>,
    managed: BTreeMap<String, otto_core::config::ApprovalConfig>,
}
fn key(scope: &Scope) -> SessionKey {
    (scope.workspace.clone(), scope.session_id.clone())
}
fn effective_settings(
    state: &ReviewState,
    workspace: &str,
    mut settings: ApprovalSettings,
) -> ApprovalSettings {
    if let Some(forced) = state.managed.get(workspace) {
        settings.policy = forced.policy.unwrap_or(settings.policy);
        settings.reviewer = forced.reviewer.unwrap_or(settings.reviewer);
    }
    settings
}
fn slots() -> &'static Arc<tokio::sync::Semaphore> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
}

impl Service {
    /// Reopen only an unstarted automatic authorization whose authority has
    /// changed. The original immutable operation and old journal event survive.
    pub fn revalidate_authorization(&self, request: &Request) -> Result<Request> {
        let mut current = self.store.load(&request.id)?;
        loop {
            if current.scope != request.scope || current.subject != request.subject {
                return Err(Error::Conflict);
            }
            if current.state == State::Approved
                && current.application == ApplicationState::NotStarted
                && current
                    .expires_at
                    .is_some_and(|time| time <= chrono::Utc::now().timestamp())
            {
                return self.mutate(&current.id, &current.scope, |request| {
                    if request.state != State::Approved
                        || request.application != ApplicationState::NotStarted
                    {
                        return Ok(false);
                    }
                    request.state = State::Expired;
                    Ok(true)
                });
            }
            if current.state != State::Approved
                || current.application != ApplicationState::NotStarted
                || current.kind != Kind::Authorization
                || current.decision.as_ref().is_none_or(|d| {
                    !matches!(
                        d.transport.as_str(),
                        "auto-review" | "approval-rule" | "yolo-mode" | "bounded-authority"
                    )
                })
            {
                return Ok(current);
            }
            let state = self.review_state.read().map_err(|_| Error::Unavailable)?;
            match self.store.authorization_current(&current) {
                Ok(true) => return Ok(current),
                Ok(false) => {}
                Err(Error::Conflict) => {
                    drop(state);
                    current = self.store.load(&request.id)?;
                    continue;
                }
                Err(e) => return Err(e),
            }
            let settings = self.store.settings(&current.scope)?.unwrap_or_default();
            let config = state
                .configs
                .get(&current.scope.workspace)
                .cloned()
                .unwrap_or_default();
            let mut next = current.clone();
            next.revision = current.revision.checked_add(1).ok_or(Error::Conflict)?;
            next.state = State::Pending;
            next.decision = None;
            let mut metadata = Self::metadata(
                &state,
                &current.scope,
                settings.revision,
                ReviewStatus::HumanRequired,
            );
            metadata.assessment=Some(ReviewAssessment {risk:RiskLevel::Medium,user_authorization:UserAuthorization::Unknown,outcome:ReviewOutcome::Ask,reason:"Authorization context or policy changed before dispatch; review the exact operation again.".into()});
            next.review = Some(metadata);
            if settings.policy == ApprovalPolicy::Never
                || super::policy::rule(&next, &config) == Some(RuleDecision::Deny)
            {
                let revision = next.revision;
                Self::record_decision(
                    &mut next,
                    revision,
                    false,
                    "Current approval policy forbids the unstarted operation.",
                    &Actor {
                        identity: "approval-policy".into(),
                        transport: if settings.policy == ApprovalPolicy::Never {
                            "never-policy"
                        } else {
                            "approval-rule"
                        }
                        .into(),
                    },
                    false,
                )?;
                let metadata = next.review.as_mut().unwrap();
                metadata.status = ReviewStatus::Completed;
                metadata.assessment.as_mut().unwrap().outcome = ReviewOutcome::Deny;
            }
            match self
                .store
                .commit(&current, &next, repository::ClaimChange::None, false)
            {
                Ok(request) => {
                    self.changed();
                    return Ok(request);
                }
                Err(Error::Conflict) => {
                    drop(state);
                    current = self.store.load(&request.id)?
                }
                Err(error) => return Err(error),
            }
        }
    }
    pub fn register_reviewer(
        &self,
        workspace: &str,
        root_session: &str,
        provider: Arc<dyn ReviewProvider>,
    ) -> Result<()> {
        if workspace.is_empty() || root_session.is_empty() {
            return Err(Error::Invalid("reviewer requires a root session"));
        }
        self.review_state
            .write()
            .map_err(|_| Error::Unavailable)?
            .providers
            .insert((workspace.into(), root_session.into()), provider);
        // A new native reviewer is a dependency change, so previously missing
        // or failed assessments may be retried. Ordinary polls retain a human
        // fallback rather than repeatedly calling the same failing provider.
        for request in self.store.pending()? {
            if request.scope.workspace != workspace
                || request.scope.session_id != root_session
                || super::policy::human_gate(&request)
            {
                continue;
            }
            let settings = self.settings(&request.scope)?;
            if settings.policy != ApprovalPolicy::OnRequest
                || settings.reviewer != ApprovalReviewer::AutoReview
            {
                continue;
            }
            self.mutate(&request.id, &request.scope, |current| {
                if current.state == State::Pending
                    && let Some(review) = current.review.as_mut()
                    && review.status == ReviewStatus::HumanRequired
                    && review.assessment.is_none()
                {
                    review.status = ReviewStatus::Queued;
                    return Ok(true);
                }
                Ok(false)
            })?;
        }
        self.refresh_pending(workspace, root_session)
    }
    pub fn set_review_context(
        &self,
        workspace: &str,
        root_session: &str,
        mut context: ReviewContext,
    ) -> Result<()> {
        if workspace.is_empty() || root_session.is_empty() {
            return Err(Error::Invalid("review context requires a root session"));
        }
        for text in context
            .trusted_user_messages
            .iter()
            .chain(&context.instructions)
        {
            self.check_content(text)?;
        }
        if context.evidence_digest.is_empty() || context.policy_digest.is_empty() {
            return Err(Error::Invalid("review context requires digests"));
        }
        let mut state = self.review_state.write().map_err(|_| Error::Unavailable)?;
        let config = state.configs.get(workspace).cloned().unwrap_or_default();
        context.policy_digest = digest(
            &serde_json::to_string(&(&context.policy_digest, &config))
                .map_err(|_| Error::Unavailable)?,
        );
        let scope = Scope {
            workspace: workspace.into(),
            session_id: root_session.into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        match self
            .store
            .review_context(&scope, &context.evidence_digest, &context.policy_digest)
        {
            Ok(()) => {}
            Err(Error::Unavailable) => {
                state.contexts.remove(&key(&scope));
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        state.contexts.insert(key(&scope), context);
        drop(state);
        self.refresh_pending(workspace, root_session)
    }
    pub fn set_policy_config(
        &self,
        workspace: &str,
        config: otto_core::config::ApprovalConfig,
    ) -> Result<()> {
        config
            .validate()
            .map_err(|_| Error::Invalid("invalid approval policy configuration"))?;
        let mut state = self.review_state.write().map_err(|_| Error::Unavailable)?;
        if state.configs.get(workspace) == Some(&config) {
            return Ok(());
        }
        let config_digest =
            digest(&serde_json::to_string(&config).map_err(|_| Error::Unavailable)?);
        match self
            .store
            .policy_config(workspace, config.settings(), &config_digest)
        {
            Ok(()) => {}
            Err(Error::Unavailable) => {
                state.configs.insert(workspace.into(), config);
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        for ((w, session), context) in state
            .contexts
            .iter_mut()
            .filter(|((w, _), _)| w == workspace)
        {
            context.policy_digest = digest(&format!("{}:{config_digest}", context.policy_digest));
            self.store.review_context(
                &Scope {
                    workspace: w.clone(),
                    session_id: session.clone(),
                    task_id: String::new(),
                    run_id: String::new(),
                },
                &context.evidence_digest,
                &context.policy_digest,
            )?;
        }
        state.configs.insert(workspace.into(), config);
        drop(state);
        self.refresh_pending(workspace, "")
    }
    pub fn set_managed_policy(
        &self,
        workspace: &str,
        forced: otto_core::config::ApprovalConfig,
    ) -> Result<()> {
        forced
            .validate()
            .map_err(|_| Error::Invalid("invalid managed approval policy"))?;
        let mut state = self.review_state.write().map_err(|_| Error::Unavailable)?;
        match self.store.managed_policy(workspace, &forced) {
            Ok(()) | Err(Error::Unavailable) => {}
            Err(error) => return Err(error),
        }
        state.managed.insert(workspace.into(), forced);
        drop(state);
        self.refresh_pending(workspace, "")
    }
    pub fn settings(&self, scope: &Scope) -> Result<ApprovalSettings> {
        if scope.session_id.is_empty() {
            return Ok(ApprovalSettings::from_mode(ApprovalMode::Manual));
        }
        let state = self.review_state.read().map_err(|_| Error::Unavailable)?;
        let settings = self.store.settings(scope)?.unwrap_or_else(|| {
            state
                .configs
                .get(&scope.workspace)
                .map(|c| c.settings())
                .unwrap_or_default()
        });
        Ok(effective_settings(&state, &scope.workspace, settings))
    }
    pub fn set_settings(
        &self,
        scope: &Scope,
        expected_revision: u64,
        policy: ApprovalPolicy,
        reviewer: ApprovalReviewer,
    ) -> Result<ApprovalSettings> {
        if scope.workspace.is_empty() || scope.session_id.is_empty() || !scope.task_id.is_empty() {
            return Err(Error::Invalid("approval settings require a root session"));
        }
        let state = self.review_state.write().map_err(|_| Error::Unavailable)?;
        if state.managed.get(&scope.workspace).is_some_and(|forced| {
            forced.policy.is_some_and(|p| p != policy)
                || forced.reviewer.is_some_and(|r| r != reviewer)
        }) {
            return Err(Error::Invalid(
                "approval settings conflict with managed policy",
            ));
        }
        let next = ApprovalSettings {
            policy,
            reviewer,
            revision: expected_revision.checked_add(1).ok_or(Error::Conflict)?,
        };
        let settings = self.store.set_settings(scope, expected_revision, next)?;
        drop(state);
        self.refresh_pending(&scope.workspace, &scope.session_id)?;
        self.changed();
        Ok(settings)
    }
    fn refresh_pending(&self, workspace: &str, session: &str) -> Result<()> {
        for request in self.store.pending()? {
            if request.scope.workspace == workspace
                && (session.is_empty() || request.scope.session_id == session)
            {
                self.route_request(&request.id, &request.scope)?;
            }
        }
        self.changed();
        Ok(())
    }
    pub(super) fn creation_review(&self, request: &Request) -> Result<ReviewMetadata> {
        let state = self.review_state.read().map_err(|_| Error::Unavailable)?;
        let settings = self.store.settings(&request.scope)?.unwrap_or_else(|| {
            state
                .configs
                .get(&request.scope.workspace)
                .map(|c| c.settings())
                .unwrap_or_default()
        });
        let context = state.contexts.get(&key(&request.scope));
        let settings = effective_settings(&state, &request.scope.workspace, settings);
        let config = state
            .configs
            .get(&request.scope.workspace)
            .cloned()
            .unwrap_or_default();
        let rule = super::policy::rule(request, &config);
        let bounded = request.auto_decidable();
        let deterministic = !request.scope.session_id.is_empty()
            && (rule == Some(RuleDecision::Deny)
                || settings.policy == ApprovalPolicy::Never
                || (rule != Some(RuleDecision::Ask)
                    && (settings.reviewer == ApprovalReviewer::Bypass
                        || (bounded && settings.reviewer == ApprovalReviewer::AutoReview)
                        || (rule == Some(RuleDecision::Allow)
                            && !super::policy::human_gate(request)))));
        let status = if !deterministic
            && (request.scope.session_id.is_empty()
                || (settings.policy == ApprovalPolicy::OnRequest
                    && settings.reviewer != ApprovalReviewer::Bypass
                    && (rule == Some(RuleDecision::Ask)
                        || settings.reviewer == ApprovalReviewer::User
                        || super::policy::human_gate(request)
                        || !state.providers.contains_key(&key(&request.scope))
                        || context.is_none_or(|c| c.trusted_user_messages.is_empty()))))
        {
            ReviewStatus::HumanRequired
        } else {
            ReviewStatus::Queued
        };
        Ok(Self::metadata(
            &state,
            &request.scope,
            settings.revision,
            status,
        ))
    }
    fn metadata(
        state: &ReviewState,
        scope: &Scope,
        revision: u64,
        status: ReviewStatus,
    ) -> ReviewMetadata {
        let context = state.contexts.get(&key(scope));
        ReviewMetadata {
            status,
            assessment: None,
            settings_revision: revision,
            evidence_digest: context
                .map_or("missing", |context| &context.evidence_digest)
                .into(),
            policy_digest: context
                .map(|context| context.policy_digest.clone())
                .unwrap_or_else(|| {
                    digest(
                        &serde_json::to_string(
                            &state
                                .configs
                                .get(&scope.workspace)
                                .cloned()
                                .unwrap_or_default(),
                        )
                        .expect("approval config encodes"),
                    )
                }),
        }
    }
    pub(super) fn route_request(&self, id: &str, scope: &Scope) -> Result<Request> {
        let state = self.review_state.read().map_err(|_| Error::Unavailable)?;
        let settings = self.store.settings(scope)?.unwrap_or_else(|| {
            state
                .configs
                .get(&scope.workspace)
                .map(|c| c.settings())
                .unwrap_or_default()
        });
        let config = state
            .configs
            .get(&scope.workspace)
            .cloned()
            .unwrap_or_default();
        let settings = effective_settings(&state, &scope.workspace, settings);
        let request = self.store.load(id)?;
        if &request.scope != scope {
            return Err(Error::Unauthorized);
        }
        if request.state != State::Pending {
            return Ok(request);
        }
        let session_owned = !scope.session_id.is_empty();
        let rule = super::policy::rule(&request, &config);
        let bounded = request.kind == Kind::Authorization
            && request.authority == Authority::Bounded
            && !request.persistent
            && !super::policy::human_gate(&request);
        let decision = if !session_owned {
            None
        } else if rule == Some(RuleDecision::Deny) {
            Some((
                false,
                "approval-rule",
                "An explicit approval rule forbids this operation.",
            ))
        } else if rule == Some(RuleDecision::Ask) {
            if settings.policy == ApprovalPolicy::Never {
                Some((
                    false,
                    "never-policy",
                    "Additional authority is disabled by the NEVER policy.",
                ))
            } else {
                None
            }
        } else if bounded
            && (settings.reviewer != ApprovalReviewer::User
                || settings.policy == ApprovalPolicy::Never)
        {
            Some((
                true,
                "bounded-authority",
                "The operation stays within existing confined authority.",
            ))
        } else if rule == Some(RuleDecision::Allow) && !super::policy::human_gate(&request) {
            Some((
                true,
                "approval-rule",
                "An explicit approval rule allows this exact operation.",
            ))
        } else if settings.policy == ApprovalPolicy::Never {
            Some((
                false,
                "never-policy",
                "Additional authority is disabled by the NEVER policy.",
            ))
        } else if settings.reviewer == ApprovalReviewer::Bypass {
            Some((true, "yolo-mode", "Automatically approved in YOLO mode."))
        } else {
            None
        };
        let mut next = request.clone();
        let needs_human = !session_owned
            || settings.reviewer == ApprovalReviewer::User
            || super::policy::human_gate(&request)
            || request.review.is_none()
            || super::policy::rule(&request, &config) == Some(RuleDecision::Ask)
            || !state.providers.contains_key(&key(scope))
            || state
                .contexts
                .get(&key(scope))
                .is_none_or(|context| context.trusted_user_messages.is_empty());
        if let Some((allow, transport, reason)) = decision {
            let mut metadata =
                Self::metadata(&state, scope, settings.revision, ReviewStatus::Completed);
            metadata.evidence_digest = "existing-policy".into();
            metadata.policy_digest =
                digest(&serde_json::to_string(&config).map_err(|_| Error::Unavailable)?);
            if transport == "approval-rule" {
                metadata.evidence_digest = "explicit-rule".into();
                metadata.policy_digest =
                    digest(&serde_json::to_string(&config).map_err(|_| Error::Unavailable)?);
            }
            next.review = Some(metadata);
            Self::record_decision(
                &mut next,
                request.revision,
                allow,
                reason,
                &Actor {
                    identity: "approval-policy".into(),
                    transport: transport.into(),
                },
                false,
            )?;
            let result = self
                .store
                .commit(&request, &next, repository::ClaimChange::None, true)?;
            self.changed();
            return Ok(result);
        }
        // An assessment that asked the user remains a human choice. Legacy
        // pending rows never acquire new automatic authority on restart.
        if request.review.is_none() {
            return Ok(request);
        }
        let captured = Self::metadata(&state, scope, settings.revision, ReviewStatus::Queued);
        let desired = if needs_human
            || request.review.as_ref().is_some_and(|r| {
                r.status == ReviewStatus::HumanRequired
                    && (r.assessment.is_some()
                        || r.matches(
                            captured.settings_revision,
                            &captured.evidence_digest,
                            &captured.policy_digest,
                        ))
            }) {
            ReviewStatus::HumanRequired
        } else {
            ReviewStatus::Queued
        };
        let metadata = Self::metadata(&state, scope, settings.revision, desired);
        if request.review.as_ref().is_some_and(|r| {
            r.matches(
                metadata.settings_revision,
                &metadata.evidence_digest,
                &metadata.policy_digest,
            ) && (r.status == desired
                || (r.status == ReviewStatus::Reviewing && desired == ReviewStatus::Queued))
        }) {
            return Ok(request);
        }
        next.review = Some(metadata);
        if repository::pending_basis_changed(&request, &next) {
            next.revision = request.revision.checked_add(1).ok_or(Error::Conflict)?;
        }
        let result = self
            .store
            .commit(&request, &next, repository::ClaimChange::None, false)?;
        self.changed();
        Ok(result)
    }
    pub(super) fn schedule_reviews(self: &Arc<Self>) {
        let Ok(requests) = self.store.pending() else {
            return;
        };
        for request in requests.into_iter().filter(|r| {
            r.review
                .as_ref()
                .is_some_and(|m| matches!(m.status, ReviewStatus::Queued | ReviewStatus::Reviewing))
        }) {
            let service = Arc::downgrade(self);
            tokio::spawn(async move {
                if let Some(service) = service.upgrade() {
                    let _ = service
                        .review_once(&request, &CancellationToken::new())
                        .await;
                }
            });
        }
    }
    pub(super) async fn review_once(
        &self,
        request: &Request,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // No waiters accumulate when all four provider calls are occupied.
        let Ok(_permit) = slots().clone().try_acquire_owned() else {
            return Ok(());
        };
        let (pid, started) = crate::subagent::record::current_process();
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let lease = ReviewLease {
            owner: format!("{}:{nonce}", self.owner),
            pid,
            started,
            expires_at: chrono::Utc::now().timestamp() + 35,
        };
        if !self.store.take_review_lease(request, &lease)? {
            return Ok(());
        };
        let result = self.perform_review(request, cancel, &lease.owner).await;
        let released = self.store.release_review_lease(&request.id, &lease.owner);
        result.and(released)
    }
    async fn perform_review(
        &self,
        request: &Request,
        cancel: &CancellationToken,
        lease_owner: &str,
    ) -> Result<()> {
        let (provider, context, expected) = {
            let state = self.review_state.read().map_err(|_| Error::Unavailable)?;
            let Some(provider) = state.providers.get(&key(&request.scope)).cloned() else {
                return Ok(());
            };
            let Some(context) = state.contexts.get(&key(&request.scope)).cloned() else {
                return Ok(());
            };
            let expected = self.store.load(&request.id)?;
            let Some(metadata) = &expected.review else {
                return Ok(());
            };
            let settings = self.store.settings(&expected.scope)?.unwrap_or_default();
            if expected.state != State::Pending
                || settings.reviewer != ApprovalReviewer::AutoReview
                || settings.policy != ApprovalPolicy::OnRequest
                || !metadata.matches(
                    settings.revision,
                    &context.evidence_digest,
                    &context.policy_digest,
                )
            {
                return Ok(());
            };
            let mut next = expected.clone();
            next.review.as_mut().unwrap().status = ReviewStatus::Reviewing;
            let expected = if next != expected {
                self.store
                    .commit(&expected, &next, repository::ClaimChange::None, false)?
            } else {
                expected
            };
            self.changed();
            (provider, context, expected)
        };
        let hydrated = self.hydrate(expected.clone());
        let denied_repeatedly = self
            .store
            .review_denials(&expected.scope, &context.evidence_digest)?
            >= 3;
        let assessed = if denied_repeatedly {
            Some(ReviewAssessment {risk: RiskLevel::High,user_authorization: UserAuthorization::Unknown,outcome: ReviewOutcome::Deny,reason:"Three review denials stop further escalation attempts for this task; choose a safer path or obtain a concrete user decision.".into()})
        } else if let Ok(hydrated) = hydrated {
            if self.check_content(&hydrated.subject.details).is_err()
                || serde_json::to_vec(&(
                    &hydrated,
                    &context.trusted_user_messages,
                    &context.instructions,
                ))
                .map_err(|_| Error::Unavailable)?
                .len()
                    > 65536
            {
                None
            } else {
                let review_cancel = cancel.child_token();
                let mut changes = self.changes();
                let future = tokio::time::timeout(
                    Duration::from_secs(30),
                    provider.assess(&hydrated, &context, &review_cancel),
                );
                tokio::pin!(future);
                let result = loop {
                    tokio::select! {
                        () = cancel.cancelled() => break None,
                        result = &mut future => break result.ok().and_then(|r|r.ok()),
                        _ = changes.changed() => { if self.store.load(&expected.id)? != expected {break None} },
                    }
                };
                review_cancel.cancel();
                result
            }
        } else {
            None
        };
        if cancel.is_cancelled() {
            return Ok(());
        };
        let assessed = assessed
            .filter(|assessment| {
                !assessment.reason.trim().is_empty()
                    && assessment.reason.len() <= 2048
                    && self.check_content(&assessment.reason).is_ok()
            })
            .map(|assessment| super::policy::constrained(assessment, &expected));
        let state = self.review_state.read().map_err(|_| Error::Unavailable)?;
        let current_context = state.contexts.get(&key(&expected.scope));
        if current_context.is_none_or(|c| {
            c.evidence_digest != context.evidence_digest || c.policy_digest != context.policy_digest
        }) {
            return Ok(());
        };
        let mut next = expected.clone();
        let metadata = next.review.as_mut().unwrap();
        let outcome = assessed
            .as_ref()
            .map_or(ReviewOutcome::Ask, |assessment| assessment.outcome);
        metadata.status = if outcome == ReviewOutcome::Ask {
            ReviewStatus::HumanRequired
        } else {
            ReviewStatus::Completed
        };
        metadata.assessment = assessed;
        if outcome != ReviewOutcome::Ask {
            let reason = metadata.assessment.as_ref().unwrap().reason.clone();
            Self::record_decision(
                &mut next,
                expected.revision,
                outcome == ReviewOutcome::Allow,
                &reason,
                &Actor {
                    identity: "isolated-reviewer".into(),
                    transport: "auto-review".into(),
                },
                false,
            )?;
        }
        match self.store.commit_review(&expected, &next, lease_owner) {
            Ok(_) => {
                self.changed();
                Ok(())
            }
            Err(Error::Conflict) => Ok(()),
            Err(error) => Err(error),
        }
    }
}
