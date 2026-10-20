//! Inherent methods of `JsonStore`: file persistence and claim bookkeeping.

use super::*;

impl JsonStore {
    pub(super) fn new(path: PathBuf) -> Self {
        let data = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
        } else {
            Data::default()
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }
    pub(super) fn save(&self, data: &Data) -> approval::Result<()> {
        let temporary = self.path.with_extension("pending");
        std::fs::write(
            &temporary,
            serde_json::to_vec(data).map_err(|_| approval::Error::Unavailable)?,
        )
        .map_err(|_| approval::Error::Unavailable)?;
        std::fs::File::open(&temporary)
            .and_then(|f| f.sync_all())
            .map_err(|_| approval::Error::Unavailable)?;
        std::fs::rename(temporary, &self.path).map_err(|_| approval::Error::Unavailable)?;
        std::fs::File::open(self.path.parent().unwrap())
            .and_then(|f| f.sync_all())
            .map_err(|_| approval::Error::CommitUnknown)
    }
    pub(super) fn mutate<R>(
        &self,
        operation: impl FnOnce(&mut Data) -> approval::Result<R>,
    ) -> approval::Result<R> {
        let mut data = self.data.lock().map_err(|_| approval::Error::Unavailable)?;
        let mut next = data.clone();
        let result = operation(&mut next)?;
        self.save(&next)?;
        *data = next;
        Ok(result)
    }
    pub(super) fn put(data: &mut Data, value: ApprovalRecord) -> approval::Result<()> {
        let (id, revision) = value.metadata().map_err(|_| approval::Error::Unavailable)?;
        let payload = serde_json::to_vec(&value).map_err(|_| approval::Error::Unavailable)?;
        data.records.insert(
            id.clone(),
            Record {
                id,
                revision,
                payload,
            },
        );
        Ok(())
    }
    pub(super) fn values(data: &Data) -> Vec<ApprovalRecord> {
        data.records
            .values()
            .map(|r| serde_json::from_slice(&r.payload).unwrap())
            .collect()
    }
    pub(super) fn requests(data: &Data) -> Vec<Request> {
        Self::values(data)
            .into_iter()
            .filter_map(|value| {
                if let ApprovalRecord::Request { request, .. } = value {
                    Some(approval::codec::decode_persisted(&request).unwrap())
                } else {
                    None
                }
            })
            .collect()
    }
    pub(super) fn events_from(data: &Data) -> Vec<Change> {
        let mut events: Vec<_> = Self::values(data)
            .into_iter()
            .filter_map(|value| {
                if let ApprovalRecord::Event { sequence, request } = value {
                    Some(Change {
                        sequence,
                        request: approval::codec::decode_persisted(&request).unwrap(),
                    })
                } else {
                    None
                }
            })
            .collect();
        events.sort_by_key(|event| event.sequence);
        events
    }
    pub(super) fn append_event(data: &mut Data, request: &Request) -> approval::Result<()> {
        let sequence = Self::events_from(data).last().map_or(1, |e| e.sequence + 1);
        Self::put(
            data,
            ApprovalRecord::Event {
                sequence,
                request: serde_json::to_string(request)
                    .map_err(|_| approval::Error::Unavailable)?,
            },
        )
    }
    pub(super) fn claim_in(data: &Data, id: &str) -> Option<Claim> {
        data.claims.get(id).cloned().map(Claim::from).or_else(|| {
            Self::values(data)
                .into_iter()
                .find_map(|value| match value {
                    ApprovalRecord::InterruptedClaim {
                        approval_id,
                        recovery,
                    } if approval_id == id => Some(Claim {
                        owner: "interrupted".into(),
                        pid: 0,
                        started: String::new(),
                        native: recovery == "native",
                    }),
                    _ => None,
                })
        })
    }
    pub(super) fn settings_in(data: &Data, scope: &Scope) -> Option<ApprovalSettings> {
        let mut fallback = None;
        let mut legacy_session = None;
        for value in Self::values(data) {
            match value {
                ApprovalRecord::Settings {
                    workspace,
                    session_id,
                    settings,
                } if workspace == scope.workspace => {
                    if session_id == scope.session_id {
                        return Some(settings);
                    }
                    if session_id.is_empty() {
                        fallback = Some(settings);
                    }
                }
                ApprovalRecord::Mode {
                    workspace,
                    session_id,
                    mode,
                } if workspace == scope.workspace && session_id == scope.session_id => {
                    legacy_session = Some(ApprovalSettings::from_mode(mode.into()));
                }
                _ => (),
            }
        }
        legacy_session.or(fallback)
    }
    pub(super) fn context_in(data: &Data, scope: &Scope) -> Option<(String, String)> {
        Self::values(data)
            .into_iter()
            .find_map(|value| match value {
                ApprovalRecord::ReviewContext {
                    workspace,
                    session_id,
                    evidence_digest,
                    policy_digest,
                } if workspace == scope.workspace && session_id == scope.session_id => {
                    Some((evidence_digest, policy_digest))
                }
                _ => None,
            })
    }
    pub(super) fn check_managed(
        data: &Data,
        scope: &Scope,
        settings: ApprovalSettings,
    ) -> approval::Result<()> {
        for value in Self::values(data) {
            if let ApprovalRecord::ManagedPolicy {
                workspace,
                policy,
                reviewer,
            } = value
                && workspace == scope.workspace
                && (policy.is_some_and(|p| p != settings.policy)
                    || reviewer.is_some_and(|r| r != settings.reviewer))
            {
                return Err(approval::Error::Conflict);
            }
        }
        Ok(())
    }
    pub(super) fn automatic_current(data: &Data, request: &Request, review_owner: bool) -> bool {
        if request.scope.session_id.is_empty() {
            return false;
        }
        let settings = Self::settings_in(data, &request.scope).unwrap_or_default();
        let metadata = request.review.as_ref();
        let transport = request.decision.as_ref().map(|d| d.transport.as_str());
        let mut workspace_scope = request.scope.clone();
        workspace_scope.session_id.clear();
        if matches!(
            transport,
            Some("bounded-authority" | "approval-rule" | "never-policy" | "yolo-mode")
        ) {
            let policy = Self::context_in(data, &workspace_scope)
                .map(|(_, p)| p)
                .unwrap_or_else(|| {
                    approval::digest(
                        &serde_json::to_string(&otto_core::config::ApprovalConfig::default())
                            .unwrap(),
                    )
                });
            if metadata.is_none_or(|m| {
                m.settings_revision != settings.revision || m.policy_digest != policy
            }) {
                return false;
            }
        }
        if transport == Some("bounded-authority") {
            return request.kind == otto_core::approval::Kind::Authorization
                && request.authority == otto_core::approval::Authority::Bounded
                && !request.persistent
                && (settings.policy == ApprovalPolicy::Never
                    || settings.reviewer != ApprovalReviewer::User);
        }
        if transport == Some("approval-rule") {
            return metadata.is_some_and(|m| {
                m.settings_revision == settings.revision
                    && Self::context_in(data, &workspace_scope)
                        .is_some_and(|(_, p)| p == m.policy_digest)
            });
        }
        if settings.policy == ApprovalPolicy::Never {
            return request.state == State::Rejected
                && metadata.is_some_and(|m| m.settings_revision == settings.revision);
        }
        if settings.reviewer == ApprovalReviewer::Bypass {
            return metadata.is_none_or(|m| m.settings_revision == settings.revision);
        }
        if settings.reviewer != ApprovalReviewer::AutoReview {
            return false;
        }
        let Some(metadata) = metadata else {
            return false;
        };
        if metadata.settings_revision != settings.revision {
            return false;
        }
        review_owner
            && Self::context_in(data, &request.scope)
                .is_some_and(|(e, p)| metadata.matches(settings.revision, &e, &p))
    }
    pub(super) fn ready(&self) -> bool {
        let data = self.data.lock().unwrap();
        data.initialized || data.sealed
    }
}

impl JsonStore {
    pub(super) fn commit_owned(
        &self,
        expected: &Request,
        next: &Request,
        claim: ClaimChange,
        automatic: bool,
        review_owner: Option<&str>,
    ) -> approval::Result<Request> {
        let advanced_once = expected.revision.checked_add(1) == Some(next.revision);
        let basis_changed = match (&expected.review, &next.review) {
            (Some(old), Some(new)) => {
                old.settings_revision != new.settings_revision
                    || old.evidence_digest != new.evidence_digest
                    || old.policy_digest != new.policy_digest
            }
            _ => false,
        };
        let basis_refresh = expected.state == State::Pending
            && next.state == State::Pending
            && expected.application == next.application
            && advanced_once
            && basis_changed;
        let unstarted_reauthorization = expected.state == State::Approved
            && expected.application == ApplicationState::NotStarted
            && next.application == ApplicationState::NotStarted
            && next.state == State::Pending
            && next.decision.is_none()
            && advanced_once
            && next
                .review
                .as_ref()
                .is_some_and(|m| m.status == otto_core::approval::ReviewStatus::HumanRequired);
        if next.id != expected.id
            || next.scope != expected.scope
            || (next.revision != expected.revision && !basis_refresh && !unstarted_reauthorization)
            || next.schema_version != expected.schema_version
            || next.subject != expected.subject
        {
            return Err(approval::Error::Conflict);
        }
        self.mutate(|data| {
            let current = Self::requests(data)
                .into_iter()
                .find(|r| r.id == expected.id)
                .ok_or(approval::Error::NotFound)?;
            if current != *expected {
                return Err(approval::Error::Conflict);
            }
            if !automatic
                && expected.state == State::Pending
                && matches!(next.state, State::Approved | State::Rejected)
                && !expected.scope.session_id.is_empty()
            {
                let settings = Self::settings_in(data, &expected.scope).unwrap_or_default();
                if settings.policy == ApprovalPolicy::Never && next.state == State::Approved {
                    return Err(approval::Error::Conflict);
                }
                if let Some(metadata) = &expected.review {
                    if metadata.settings_revision != settings.revision {
                        return Err(approval::Error::Conflict);
                    }
                    let current_basis = if let Some((evidence, policy)) =
                        Self::context_in(data, &expected.scope)
                    {
                        metadata.evidence_digest == evidence && metadata.policy_digest == policy
                    } else {
                        let mut workspace_scope = expected.scope.clone();
                        workspace_scope.session_id.clear();
                        let policy = Self::context_in(data, &workspace_scope)
                            .map(|(_, p)| p)
                            .unwrap_or_else(|| {
                                approval::digest(
                                    &serde_json::to_string(
                                        &otto_core::config::ApprovalConfig::default(),
                                    )
                                    .unwrap(),
                                )
                            });
                        metadata.evidence_digest == "missing" && metadata.policy_digest == policy
                    };
                    if !current_basis {
                        return Err(approval::Error::Conflict);
                    }
                }
            }
            let starting_auto = expected.state == State::Approved
                && expected.application == ApplicationState::NotStarted
                && next.application == ApplicationState::Applying
                && expected.decision.as_ref().is_some_and(|d| {
                    matches!(
                        d.transport.as_str(),
                        "auto-review" | "approval-rule" | "yolo-mode" | "bounded-authority"
                    )
                });
            if (automatic || starting_auto)
                && !Self::automatic_current(data, next, review_owner.is_some() || starting_auto)
            {
                return Err(approval::Error::Conflict);
            }
            if let Some(owner) = review_owner
                && data
                    .review_leases
                    .get(&expected.id)
                    .is_none_or(|(actual, _, _, expiry)| {
                        actual != owner || *expiry <= chrono::Utc::now().timestamp()
                    })
            {
                return Err(approval::Error::Conflict);
            }
            match claim {
                ClaimChange::None => (),
                ClaimChange::Start(claim) => {
                    if Self::claim_in(data, &expected.id).is_some() {
                        return Err(approval::Error::Conflict);
                    }
                    data.claims
                        .insert(expected.id.clone(), StoredClaim::from(&claim));
                }
                ClaimChange::Finish(owner) => {
                    if Self::claim_in(data, &expected.id).is_none_or(|claim| claim.owner != owner) {
                        return Err(approval::Error::Conflict);
                    }
                }
            }
            let subject_key = Self::values(data)
                .into_iter()
                .find_map(|value| match value {
                    ApprovalRecord::Request {
                        subject_key,
                        request,
                    } if approval::codec::decode_persisted(&request).unwrap().id == expected.id => {
                        Some(subject_key)
                    }
                    _ => None,
                })
                .unwrap();
            let record = &data.records[&format!("request:{}", expected.id)];
            let original = match serde_json::from_slice::<ApprovalRecord>(&record.payload).unwrap()
            {
                ApprovalRecord::Request { request, .. } => request,
                _ => unreachable!(),
            };
            let body = approval::codec::merge_persisted(&original, next)?;
            Self::put(
                data,
                ApprovalRecord::Request {
                    subject_key,
                    request: body.clone(),
                },
            )?;
            let sequence = Self::events_from(data)
                .last()
                .map_or(1, |event| event.sequence + 1);
            Self::put(
                data,
                ApprovalRecord::Event {
                    sequence,
                    request: body,
                },
            )?;
            if let Some(metadata) = &next.review
                && next
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.transport == "auto-review")
                && expected.state == State::Pending
                && matches!(next.state, State::Approved | State::Rejected)
            {
                let count = Self::values(data)
                    .into_iter()
                    .find_map(|value| match value {
                        ApprovalRecord::ReviewDenials {
                            workspace,
                            session_id,
                            task_id,
                            run_id,
                            evidence_digest,
                            denials,
                        } if workspace == next.scope.workspace
                            && session_id == next.scope.session_id
                            && task_id == next.scope.task_id
                            && run_id == next.scope.run_id
                            && evidence_digest == metadata.evidence_digest =>
                        {
                            Some(denials)
                        }
                        _ => None,
                    })
                    .unwrap_or(0);
                Self::put(
                    data,
                    ApprovalRecord::ReviewDenials {
                        workspace: next.scope.workspace.clone(),
                        session_id: next.scope.session_id.clone(),
                        task_id: next.scope.task_id.clone(),
                        run_id: next.scope.run_id.clone(),
                        evidence_digest: metadata.evidence_digest.clone(),
                        denials: if next.state == State::Rejected {
                            count + 1
                        } else {
                            0
                        },
                    },
                )?;
            }
            Ok(next.clone())
        })
    }
}
