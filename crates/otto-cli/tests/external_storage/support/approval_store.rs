//! `ApprovalStore` implementation of `JsonStore`.

use super::*;

impl ApprovalStore for JsonStore {
    fn mode(&self, scope: &Scope) -> approval::Result<Option<ApprovalMode>> {
        Ok(self.settings(scope)?.map(|s| s.mode()))
    }
    fn set_mode(&self, scope: &Scope, mode: ApprovalMode) -> approval::Result<()> {
        self.mutate(|data| {
            let mut settings = ApprovalSettings::from_mode(mode);
            Self::check_managed(data, scope, settings)?;
            settings.revision = Self::settings_in(data, scope)
                .unwrap_or_default()
                .revision
                .checked_add(1)
                .ok_or(approval::Error::Conflict)?;
            Self::put(
                data,
                ApprovalRecord::Settings {
                    workspace: scope.workspace.clone(),
                    session_id: scope.session_id.clone(),
                    settings,
                },
            )
        })
    }
    fn settings(&self, scope: &Scope) -> approval::Result<Option<ApprovalSettings>> {
        Ok(Self::settings_in(&self.data.lock().unwrap(), scope))
    }
    fn set_settings(
        &self,
        scope: &Scope,
        revision: u64,
        settings: ApprovalSettings,
    ) -> approval::Result<ApprovalSettings> {
        settings.validate().map_err(approval::Error::Invalid)?;
        self.mutate(|data| {
            Self::check_managed(data, scope, settings)?;
            if Self::settings_in(data, scope).unwrap_or_default().revision != revision
                || settings.revision != revision.checked_add(1).ok_or(approval::Error::Conflict)?
            {
                return Err(approval::Error::Conflict);
            }
            Self::put(
                data,
                ApprovalRecord::Settings {
                    workspace: scope.workspace.clone(),
                    session_id: scope.session_id.clone(),
                    settings,
                },
            )?;
            Ok(settings)
        })
    }
    fn review_context(&self, scope: &Scope, evidence: &str, policy: &str) -> approval::Result<()> {
        self.mutate(|data| {
            Self::put(
                data,
                ApprovalRecord::ReviewContext {
                    workspace: scope.workspace.clone(),
                    session_id: scope.session_id.clone(),
                    evidence_digest: evidence.into(),
                    policy_digest: policy.into(),
                },
            )
        })
    }
    fn review_snapshot(&self, scope: &Scope) -> approval::Result<Option<(String, String)>> {
        let data = self.data.lock().map_err(|_| approval::Error::Unavailable)?;
        Ok(Self::context_in(&data, scope))
    }
    fn policy_config(
        &self,
        workspace: &str,
        mut settings: ApprovalSettings,
        digest: &str,
    ) -> approval::Result<()> {
        self.mutate(|data| {
            let scope = Scope {
                workspace: workspace.into(),
                session_id: String::new(),
                task_id: String::new(),
                run_id: String::new(),
            };
            let changed =
                Self::context_in(data, &scope).is_none_or(|(_, previous)| previous != digest);
            if changed {
                for value in Self::values(data) {
                    if let ApprovalRecord::ReviewContext {
                        workspace: owner,
                        session_id,
                        evidence_digest,
                        ..
                    } = value
                        && owner == workspace
                        && !session_id.is_empty()
                    {
                        Self::put(
                            data,
                            ApprovalRecord::ReviewContext {
                                workspace: owner,
                                session_id,
                                evidence_digest,
                                policy_digest: format!("policy-changed:{digest}"),
                            },
                        )?;
                    }
                }
            }
            settings.revision = match Self::settings_in(data, &scope) {
                Some(old) if Self::context_in(data, &scope).is_some_and(|(_, p)| p == digest) => {
                    old.revision
                }
                Some(old) => old
                    .revision
                    .checked_add(1)
                    .ok_or(approval::Error::Conflict)?,
                None => 1,
            };
            Self::put(
                data,
                ApprovalRecord::Settings {
                    workspace: workspace.into(),
                    session_id: String::new(),
                    settings,
                },
            )?;
            Self::put(
                data,
                ApprovalRecord::ReviewContext {
                    workspace: workspace.into(),
                    session_id: String::new(),
                    evidence_digest: "policy".into(),
                    policy_digest: digest.into(),
                },
            )
        })
    }
    fn authorization_current(&self, request: &Request) -> approval::Result<bool> {
        let data = self.data.lock().unwrap();
        if Self::requests(&data).iter().find(|r| r.id == request.id) != Some(request) {
            return Err(approval::Error::Conflict);
        }
        Ok(Self::automatic_current(&data, request, true))
    }
    fn managed_policy(
        &self,
        workspace: &str,
        forced: &otto_core::config::ApprovalConfig,
    ) -> approval::Result<()> {
        self.mutate(|data| {
            let mut sessions = std::collections::BTreeSet::from([String::new()]);
            for value in Self::values(data) {
                if let ApprovalRecord::Settings {
                    workspace: owner,
                    session_id,
                    ..
                } = value
                    && owner == workspace
                {
                    sessions.insert(session_id);
                }
            }
            Self::put(
                data,
                ApprovalRecord::ManagedPolicy {
                    workspace: workspace.into(),
                    policy: forced.policy,
                    reviewer: forced.reviewer,
                },
            )?;
            for session_id in sessions {
                let scope = Scope {
                    workspace: workspace.into(),
                    session_id: session_id.clone(),
                    task_id: String::new(),
                    run_id: String::new(),
                };
                let previous = Self::settings_in(data, &scope);
                let mut settings = previous.unwrap_or_default();
                settings.policy = forced.policy.unwrap_or(settings.policy);
                settings.reviewer = forced.reviewer.unwrap_or(settings.reviewer);
                if previous.is_some_and(|old| {
                    old.policy != settings.policy || old.reviewer != settings.reviewer
                }) {
                    settings.revision = settings
                        .revision
                        .checked_add(1)
                        .ok_or(approval::Error::Conflict)?;
                }
                Self::put(
                    data,
                    ApprovalRecord::Settings {
                        workspace: workspace.into(),
                        session_id,
                        settings,
                    },
                )?;
            }
            Ok(())
        })
    }
    fn review_denials(&self, scope: &Scope, evidence: &str) -> approval::Result<u64> {
        Ok(Self::values(&self.data.lock().unwrap())
            .into_iter()
            .find_map(|value| match value {
                ApprovalRecord::ReviewDenials {
                    workspace,
                    session_id,
                    task_id,
                    run_id,
                    evidence_digest,
                    denials,
                } if workspace == scope.workspace
                    && session_id == scope.session_id
                    && task_id == scope.task_id
                    && run_id == scope.run_id
                    && evidence_digest == evidence =>
                {
                    Some(denials)
                }
                _ => None,
            })
            .unwrap_or(0))
    }
    fn take_review_lease(&self, request: &Request, lease: &ReviewLease) -> approval::Result<bool> {
        self.mutate(|data| {
            let now = chrono::Utc::now().timestamp();
            if Self::requests(data).iter().find(|r| r.id == request.id) != Some(request)
                || request.state != State::Pending
                || request.expires_at.is_some_and(|t| t <= now)
                || data
                    .review_leases
                    .get(&request.id)
                    .is_some_and(|(_, _, _, expiry)| *expiry > now)
            {
                return Ok(false);
            }
            data.review_leases.insert(
                request.id.clone(),
                (
                    lease.owner.clone(),
                    lease.pid,
                    lease.started.clone(),
                    lease.expires_at,
                ),
            );
            Ok(true)
        })
    }
    fn release_review_lease(&self, id: &str, owner: &str) -> approval::Result<()> {
        self.mutate(|data| {
            if data
                .review_leases
                .get(id)
                .is_some_and(|(actual, _, _, _)| actual == owner)
            {
                data.review_leases.remove(id);
            }
            Ok(())
        })
    }
    fn commit_review(
        &self,
        expected: &Request,
        next: &Request,
        owner: &str,
    ) -> approval::Result<Request> {
        self.commit_owned(expected, next, ClaimChange::None, true, Some(owner))
    }
    fn create(&self, key: &str, request: &Request) -> approval::Result<(Request, bool)> {
        self.mutate(|data| {
            for value in Self::values(data) {
                if let ApprovalRecord::Request {
                    subject_key,
                    request,
                } = value
                    && subject_key == key
                {
                    return Ok((approval::codec::decode_persisted(&request)?, false));
                }
            }
            if data
                .records
                .contains_key(&format!("request:{}", request.id))
            {
                return Err(approval::Error::Conflict);
            }
            Self::put(
                data,
                ApprovalRecord::Request {
                    subject_key: key.into(),
                    request: serde_json::to_string(request).unwrap(),
                },
            )?;
            Self::put(
                data,
                ApprovalRecord::Visibility {
                    approval_id: request.id.clone(),
                    workspace: request.scope.workspace.clone(),
                    session_id: request.scope.session_id.clone(),
                },
            )?;
            Self::append_event(data, request)?;
            Ok((request.clone(), true))
        })
    }
    fn load(&self, id: &str) -> approval::Result<Request> {
        Self::requests(&self.data.lock().unwrap())
            .into_iter()
            .find(|r| r.id == id)
            .ok_or(approval::Error::NotFound)
    }
    fn visible(&self, id: &str, workspace: &str, session: Option<&str>) -> approval::Result<bool> {
        let request = self.load(id)?;
        Ok(request.scope.workspace == workspace
            && session.is_none_or(|session| request.scope.session_id == session))
    }
    fn pending(&self) -> approval::Result<Vec<Request>> {
        Ok(Self::requests(&self.data.lock().unwrap())
            .into_iter()
            .filter(|r| r.state == State::Pending)
            .collect())
    }
    fn applications(&self, workspace: &str) -> approval::Result<Vec<Request>> {
        Ok(Self::requests(&self.data.lock().unwrap())
            .into_iter()
            .filter(|r| r.scope.workspace == workspace)
            .collect())
    }
    fn snapshot(&self, workspace: &str, session: Option<&str>) -> approval::Result<Snapshot> {
        let data = self.data.lock().unwrap();
        Ok(Snapshot {
            requests: Self::requests(&data)
                .into_iter()
                .filter(|r| {
                    r.state == State::Pending
                        && r.scope.workspace == workspace
                        && session.is_none_or(|session| r.scope.session_id == session)
                })
                .collect(),
            cursor: Self::events_from(&data).last().map_or(0, |r| r.sequence),
        })
    }
    fn events(
        &self,
        workspace: &str,
        session: Option<&str>,
        after: i64,
    ) -> approval::Result<Vec<Change>> {
        Ok(Self::events_from(&self.data.lock().unwrap())
            .into_iter()
            .filter(|r| {
                r.sequence > after
                    && r.request.scope.workspace == workspace
                    && session.is_none_or(|session| r.request.scope.session_id == session)
            })
            .collect())
    }
    fn claim(&self, id: &str) -> approval::Result<Option<Claim>> {
        Ok(Self::claim_in(&self.data.lock().unwrap(), id))
    }
    fn take_claim(&self, id: &str, expected: &Claim, next: &Claim) -> approval::Result<bool> {
        self.mutate(|data| {
            if Self::claim_in(data, id).as_ref() != Some(expected) {
                return Ok(false);
            }
            data.records.remove(&format!("claim:{id}"));
            data.claims.insert(id.into(), StoredClaim::from(next));
            Ok(true)
        })
    }
    fn commit(
        &self,
        expected: &Request,
        next: &Request,
        claim: ClaimChange,
        automatic: bool,
    ) -> approval::Result<Request> {
        self.commit_owned(expected, next, claim, automatic, None)
    }
}
