//! An independent JSON repository used only for public extension acceptance.
//! It does not import or wrap any Otto SQL/file backend implementation.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use otto::approval::migration::ApprovalRecord;
use otto::approval::repository::{ApprovalStore, Claim, ClaimChange, ReviewLease};
use otto::approval::{
    self, ApplicationState, ApprovalMode, ApprovalPolicy, ApprovalReviewer, ApprovalSettings,
    Change, Request, Scope, Snapshot, State,
};
use otto::runtime::storage::backend::{MigrationContext, PreparedImport};
use otto::runtime::storage::{
    Endpoint, NamespaceIdentity, RepositoryFactory, RepositoryRequirements, StorageEndpoint,
};
use otto::storage::migration::{self, *};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredClaim {
    owner: String,
    pid: i64,
    started: String,
    native: bool,
}
impl From<&Claim> for StoredClaim {
    fn from(claim: &Claim) -> Self {
        Self {
            owner: claim.owner.clone(),
            pid: claim.pid,
            started: claim.started.clone(),
            native: claim.native,
        }
    }
}
impl From<StoredClaim> for Claim {
    fn from(claim: StoredClaim) -> Self {
        Self {
            owner: claim.owner,
            pid: claim.pid,
            started: claim.started,
            native: claim.native,
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Data {
    initialized: bool,
    sealed: bool,
    migration: Option<Manifest>,
    records: BTreeMap<String, Record>,
    receipts: BTreeMap<String, ChunkReceipt>,
    #[serde(default)]
    claims: BTreeMap<String, StoredClaim>,
    #[serde(default)]
    review_leases: BTreeMap<String, (String, i64, String, i64)>,
}

pub struct JsonStore {
    path: PathBuf,
    data: Mutex<Data>,
}

impl JsonStore {
    fn new(path: PathBuf) -> Self {
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
    fn save(&self, data: &Data) -> approval::Result<()> {
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
    fn mutate<R>(
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
    fn put(data: &mut Data, value: ApprovalRecord) -> approval::Result<()> {
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
    fn values(data: &Data) -> Vec<ApprovalRecord> {
        data.records
            .values()
            .map(|r| serde_json::from_slice(&r.payload).unwrap())
            .collect()
    }
    fn requests(data: &Data) -> Vec<Request> {
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
    fn events_from(data: &Data) -> Vec<Change> {
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
    fn append_event(data: &mut Data, request: &Request) -> approval::Result<()> {
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
    fn claim_in(data: &Data, id: &str) -> Option<Claim> {
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
    fn settings_in(data: &Data, scope: &Scope) -> Option<ApprovalSettings> {
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
    fn context_in(data: &Data, scope: &Scope) -> Option<(String, String)> {
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
    fn check_managed(
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
    fn automatic_current(data: &Data, request: &Request, review_owner: bool) -> bool {
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
            return request.kind == approval::Kind::Authorization
                && request.authority == approval::Authority::Bounded
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
    fn ready(&self) -> bool {
        let data = self.data.lock().unwrap();
        data.initialized || data.sealed
    }
}

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

impl JsonStore {
    fn commit_owned(
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
                .is_some_and(|m| m.status == approval::ReviewStatus::HumanRequired);
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

pub struct JsonBackend {
    pub store: Arc<JsonStore>,
    pub identity: String,
    pub lose_import_ack: Arc<AtomicBool>,
}
impl JsonBackend {
    pub fn new(path: PathBuf, identity: &str) -> Self {
        Self {
            store: Arc::new(JsonStore::new(path)),
            identity: identity.into(),
            lose_import_ack: Arc::new(AtomicBool::new(false)),
        }
    }
    pub async fn check_import_receipts(&self) -> migration::Result<()> {
        let data = self.store.data.lock().unwrap().clone();
        let manifest = data
            .migration
            .as_ref()
            .ok_or_else(|| Error::Invalid("test import missing".into()))?;
        let receipt = data
            .receipts
            .get(&serde_json::to_string(&("approvals", "")).unwrap())
            .ok_or_else(|| Error::Invalid("test receipt missing".into()))?;
        let records = record::canonical_records("approvals", data.records.values().cloned())?;
        let command = Chunk {
            cursor: receipt.cursor.clone(),
            next_cursor: receipt.next_cursor.clone(),
            records,
            blobs: Vec::new(),
        };
        let stage = JsonStage {
            store: self.store.clone(),
            closed: AtomicBool::new(false),
            lose_ack: Arc::new(AtomicBool::new(false)),
        };
        otto::storage::conformance::import_receipts(
            &stage,
            &manifest.migration_id,
            "approvals",
            &command,
            manifest.max_chunk_bytes,
        )
        .await
    }
    fn check(
        endpoint: &StorageEndpoint,
        requirement: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
    ) -> otto::storage::Result<()> {
        if !matches!(endpoint, StorageEndpoint::Service(_))
            || requirement.domain != "approvals"
            || requirement.profile_version != 1
            || !options.is_empty()
        {
            return Err(otto::storage::Error::UnsupportedCapability(
                "independent approval profile",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl RepositoryFactory for JsonBackend {
    async fn validate(
        &self,
        endpoint: &StorageEndpoint,
        requirements: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
    ) -> otto::storage::Result<NamespaceIdentity> {
        tokio::task::yield_now().await;
        Self::check(endpoint, requirements, options)?;
        if !self.store.ready() {
            return Err(otto::storage::Error::Unavailable);
        }
        Ok(NamespaceIdentity(self.identity.clone()))
    }
    async fn initialize(
        &self,
        endpoint: &StorageEndpoint,
        domains: &[RepositoryRequirements],
        options: &BTreeMap<String, String>,
    ) -> otto::storage::Result<NamespaceIdentity> {
        for requirement in domains {
            Self::check(endpoint, requirement, options)?;
        }
        self.store
            .mutate(|data| {
                if data.initialized || data.sealed || !data.records.is_empty() {
                    return Err(approval::Error::Conflict);
                }
                data.initialized = true;
                Ok(())
            })
            .map_err(|_| otto::storage::Error::Unavailable)?;
        Ok(NamespaceIdentity(self.identity.clone()))
    }
    fn approvals(&self, endpoint: Endpoint<'_>) -> approval::Result<Arc<dyn ApprovalStore>> {
        if !self.store.ready() || endpoint.address.is_empty() {
            return Err(approval::Error::Unavailable);
        }
        Ok(self.store.clone())
    }
    async fn prepare_import(
        &self,
        endpoint: &StorageEndpoint,
        requirement: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
        context: &MigrationContext,
    ) -> migration::Result<PreparedImport> {
        Self::check(endpoint, requirement, options)
            .map_err(|_| Error::Invalid("unsupported import profile".into()))?;
        {
            let data = self.store.data.lock().unwrap();
            if data.initialized
                || data
                    .migration
                    .as_ref()
                    .is_some_and(|m| m.migration_id != context.migration_id)
                || data.migration.is_none() && !data.records.is_empty()
            {
                return Err(Error::Conflict("destination is occupied".into()));
            }
        }
        Ok(PreparedImport {
            namespace_identity: NamespaceIdentity(self.identity.clone()),
            stage: Arc::new(JsonStage {
                store: self.store.clone(),
                closed: AtomicBool::new(false),
                lose_ack: self.lose_import_ack.clone(),
            }),
        })
    }
    async fn export(
        &self,
        endpoint: &StorageEndpoint,
        requirement: &RepositoryRequirements,
        options: &BTreeMap<String, String>,
        context: &MigrationContext,
    ) -> migration::Result<Arc<dyn SnapshotExport>> {
        Self::check(endpoint, requirement, options)
            .map_err(|_| Error::Invalid("unsupported export profile".into()))?;
        context.fence.validate(&context.generation)?;
        let mut data = self.store.data.lock().unwrap().clone();
        for (id, claim) in data.claims.clone() {
            JsonStore::put(
                &mut data,
                ApprovalRecord::InterruptedClaim {
                    approval_id: id,
                    recovery: if claim.native { "native" } else { "domain" }.into(),
                },
            )
            .map_err(failure)?;
        }
        let records = record::canonical_records("approvals", data.records.values().cloned())?;
        let cursor = JsonStore::events_from(&self.store.data.lock().unwrap())
            .last()
            .map_or(0, |e| e.sequence)
            .to_string();
        let snapshot = FrozenSnapshot {
            source_identity: context.source_identity.clone(),
            generation: context.generation.clone(),
            domains: BTreeMap::from([(
                "approvals".into(),
                DomainSnapshot {
                    schema_family: "otto.approvals".into(),
                    schema_version: 1,
                    committed_cursor: cursor,
                    digest: record::digest("approvals", records.clone())?,
                },
            )]),
        };
        Ok(Arc::new(JsonSource {
            snapshot,
            records,
            fence: context.fence.clone(),
            closed: AtomicBool::new(false),
        }))
    }
}

fn chunk(records: &[Record], cursor: &str, max_bytes: usize) -> migration::Result<Chunk> {
    let index: usize = if cursor.is_empty() {
        0
    } else {
        cursor
            .parse()
            .map_err(|_| Error::Invalid("invalid export cursor".into()))?
    };
    if index > records.len() {
        return Err(Error::Invalid("export cursor outside snapshot".into()));
    }
    let value = Chunk {
        cursor: cursor.into(),
        next_cursor: (index < records.len()).then(|| (index + 1).to_string()),
        records: records.get(index).cloned().into_iter().collect(),
        blobs: Vec::new(),
    };
    chunk_hash(&value, max_bytes)?;
    Ok(value)
}

struct JsonSource {
    snapshot: FrozenSnapshot,
    records: Vec<Record>,
    fence: Arc<dyn FreezeGuard>,
    closed: AtomicBool,
}
#[async_trait]
impl SnapshotExport for JsonSource {
    async fn descriptor(&self) -> migration::Result<FrozenSnapshot> {
        self.validate_frozen(&self.snapshot).await?;
        Ok(self.snapshot.clone())
    }
    async fn validate_frozen(&self, expected: &FrozenSnapshot) -> migration::Result<()> {
        self.fence.validate(&self.snapshot.generation)?;
        if self.closed.load(Ordering::SeqCst) || expected != &self.snapshot {
            return Err(Error::Conflict("snapshot closed or changed".into()));
        }
        Ok(())
    }
    async fn read_chunk(
        &self,
        domain: &str,
        cursor: &str,
        max_bytes: usize,
    ) -> migration::Result<Chunk> {
        self.validate_frozen(&self.snapshot).await?;
        if domain != "approvals" {
            return Err(Error::Invalid("unsupported export domain".into()));
        }
        chunk(&self.records, cursor, max_bytes)
    }
    async fn close(&self) -> migration::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct JsonStage {
    store: Arc<JsonStore>,
    closed: AtomicBool,
    lose_ack: Arc<AtomicBool>,
}
fn failure(_: approval::Error) -> Error {
    Error::Unavailable("independent repository unavailable".into())
}
#[async_trait]
impl StageImport for JsonStage {
    async fn validate_plan(&self, manifest: &Manifest) -> migration::Result<()> {
        if self.closed.load(Ordering::SeqCst)
            || manifest.source.domains.len() != 1
            || manifest
                .source
                .domains
                .get("approvals")
                .is_none_or(|s| s.schema_family != "otto.approvals" || s.schema_version != 1)
        {
            return Err(Error::Invalid("invalid approval import profile".into()));
        }
        Ok(())
    }
    async fn begin(&self, manifest: &Manifest) -> migration::Result<()> {
        self.validate_plan(manifest).await?;
        self.store
            .mutate(|data| {
                if let Some(previous) = &data.migration {
                    if previous.migration_id != manifest.migration_id
                        || previous.source != manifest.source
                    {
                        return Err(approval::Error::Conflict);
                    }
                } else {
                    data.migration = Some(manifest.clone());
                }
                Ok(())
            })
            .map_err(failure)
    }
    async fn apply_chunk(
        &self,
        id: &str,
        domain: &str,
        chunk: &Chunk,
        hash: &str,
    ) -> migration::Result<ChunkReceipt> {
        let receipt = ChunkReceipt {
            cursor: chunk.cursor.clone(),
            next_cursor: chunk.next_cursor.clone(),
            sha256: hash.into(),
        };
        let key = serde_json::to_string(&(domain, &chunk.cursor)).unwrap();
        let data = self.store.data.lock().unwrap();
        let limit = data
            .migration
            .as_ref()
            .filter(|m| m.migration_id == id)
            .ok_or_else(|| Error::Conflict("migration differs".into()))?
            .max_chunk_bytes;
        if self.closed.load(Ordering::SeqCst)
            || domain != "approvals"
            || !chunk.blobs.is_empty()
            || chunk_hash(chunk, limit)? != hash
        {
            return Err(Error::Invalid("invalid import command".into()));
        }
        drop(data);
        let result = self
            .store
            .mutate(|data| {
                if let Some(prior) = data.receipts.get(&key) {
                    return if prior == &receipt {
                        Ok(prior.clone())
                    } else {
                        Err(approval::Error::Conflict)
                    };
                }
                if data.sealed {
                    return Err(approval::Error::Conflict);
                }
                for record in &chunk.records {
                    record::order(domain, record)
                        .map_err(|_| approval::Error::Invalid("invalid approval record"))?;
                    if data
                        .records
                        .insert(record.id.clone(), record.clone())
                        .is_some()
                    {
                        return Err(approval::Error::Conflict);
                    }
                }
                data.receipts.insert(key, receipt.clone());
                Ok(receipt)
            })
            .map_err(|error| match error {
                approval::Error::Conflict => Error::Conflict("import command differs".into()),
                other => failure(other),
            })?;
        if self.lose_ack.swap(false, Ordering::SeqCst) {
            return Err(Error::CommitUnknown);
        }
        Ok(result)
    }
    async fn chunk_receipt(
        &self,
        id: &str,
        domain: &str,
        cursor: &str,
    ) -> migration::Result<Option<ChunkReceipt>> {
        let data = self.store.data.lock().unwrap();
        if data
            .migration
            .as_ref()
            .is_some_and(|m| m.migration_id != id)
        {
            return Err(Error::Conflict("migration differs".into()));
        }
        Ok(data
            .receipts
            .get(&serde_json::to_string(&(domain, cursor)).unwrap())
            .cloned())
    }
    async fn verification_chunk(
        &self,
        domain: &str,
        cursor: &str,
        max_bytes: usize,
    ) -> migration::Result<Chunk> {
        if domain != "approvals" {
            return Err(Error::Invalid("unsupported verification domain".into()));
        }
        let records = record::canonical_records(
            domain,
            self.store.data.lock().unwrap().records.values().cloned(),
        )?;
        chunk(&records, cursor, max_bytes)
    }
    async fn verify(
        &self,
        manifest: &Manifest,
    ) -> migration::Result<BTreeMap<String, DomainDigest>> {
        self.validate_plan(manifest).await?;
        let data = self.store.data.lock().unwrap();
        for receipt in &manifest.progress["approvals"].chunks {
            if data
                .receipts
                .get(&serde_json::to_string(&("approvals", &receipt.cursor)).unwrap())
                != Some(receipt)
            {
                return Err(Error::Invalid("import receipt missing".into()));
            }
        }
        Ok(BTreeMap::from([(
            "approvals".into(),
            record::digest("approvals", data.records.values().cloned())?,
        )]))
    }
    async fn seal(&self, manifest: &Manifest) -> migration::Result<()> {
        if self.verify(manifest).await?["approvals"] != manifest.source.domains["approvals"].digest
        {
            return Err(Error::Invalid("import digest differs".into()));
        }
        self.store
            .mutate(|data| {
                data.sealed = true;
                Ok(())
            })
            .map_err(failure)
    }
    async fn close(&self) -> migration::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
