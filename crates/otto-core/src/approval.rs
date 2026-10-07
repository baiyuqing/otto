//! Frontend-neutral human approval contracts. Native callers supply identity,
//! time, storage and domain application; models never supply decision authority.
use serde::{Deserialize, Serialize};

mod rules;
pub use rules::{ApprovalRule, RuleDecision, command_argv, evaluate_rules};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    OnRequest,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReviewer {
    User,
    #[default]
    AutoReview,
    Bypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalSettings {
    pub policy: ApprovalPolicy,
    pub reviewer: ApprovalReviewer,
    pub revision: u64,
}

impl Default for ApprovalSettings {
    fn default() -> Self {
        Self::from_mode(ApprovalMode::Auto)
    }
}

impl ApprovalSettings {
    pub fn from_mode(mode: ApprovalMode) -> Self {
        Self {
            policy: ApprovalPolicy::OnRequest,
            reviewer: match mode {
                ApprovalMode::Manual => ApprovalReviewer::User,
                ApprovalMode::Auto => ApprovalReviewer::AutoReview,
                ApprovalMode::Yolo => ApprovalReviewer::Bypass,
            },
            revision: 1,
        }
    }

    /// Compatibility projection for clients that only understand modes.
    pub fn mode(&self) -> ApprovalMode {
        if self.policy == ApprovalPolicy::Never {
            return ApprovalMode::Auto;
        }
        match self.reviewer {
            ApprovalReviewer::User => ApprovalMode::Manual,
            ApprovalReviewer::AutoReview => ApprovalMode::Auto,
            ApprovalReviewer::Bypass => ApprovalMode::Yolo,
        }
    }

    pub fn is_automatic(&self) -> bool {
        self.policy == ApprovalPolicy::Never || self.reviewer != ApprovalReviewer::User
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.revision == 0 {
            return Err("approval settings revision must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Queued,
    Reviewing,
    HumanRequired,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAuthorization {
    Unknown,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAssessment {
    pub risk: RiskLevel,
    pub user_authorization: UserAuthorization,
    pub outcome: ReviewOutcome,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewMetadata {
    pub status: ReviewStatus,
    pub assessment: Option<ReviewAssessment>,
    pub settings_revision: u64,
    pub evidence_digest: String,
    pub policy_digest: String,
}

impl ReviewMetadata {
    /// A persisted assessment authorizes only the exact reviewed evidence and policy.
    pub fn matches(
        &self,
        settings_revision: u64,
        evidence_digest: &str,
        policy_digest: &str,
    ) -> bool {
        self.settings_revision == settings_revision
            && self.evidence_digest == evidence_digest
            && self.policy_digest == policy_digest
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.settings_revision == 0
            || self.evidence_digest.is_empty()
            || self.policy_digest.is_empty()
            || self.evidence_digest.len() > 256
            || self.policy_digest.len() > 256
            || self.assessment.as_ref().is_some_and(|assessment| {
                assessment.reason.trim().is_empty() || assessment.reason.len() > 8192
            })
            || (matches!(self.status, ReviewStatus::Queued | ReviewStatus::Reviewing)
                && self.assessment.is_some())
        {
            return Err("invalid approval review metadata");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Authorization,
    Confirmation,
}

/// The session policy that decides which requests reach the user. Selected
/// only by an admitted user action; never by a model argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Every request waits for the user.
    Manual,
    /// Only a [`Authority::Bounded`] authorization is decided automatically.
    #[default]
    Auto,
    /// Every request of the session is decided automatically, including the
    /// consents, boundary crossings and durable grants AUTO refuses.
    Yolo,
}

/// The authority a request reaches for, declared by its native producer.
/// Only a [`Authority::Bounded`] authorization may be decided by the session
/// AUTO policy; a plan review, a sandbox or workspace boundary crossing and a
/// durable grant are always a user decision. Model arguments never select this
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// A judgement about a proposal, plan or workflow step.
    Consent,
    /// This operation leaves the sandbox or the workspace, or affects an
    /// external system.
    Boundary,
    /// This operation establishes or extends durable authority.
    Durable,
    /// Inside the sandbox and the workspace, granting nothing durable.
    Bounded,
    /// No declared class. Only a request persisted before this field existed
    /// decodes this way; it is never decided automatically.
    #[default]
    Unclassified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Approved,
    Rejected,
    Withdrawn,
    Expired,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    NotRequired,
    NotStarted,
    Applying,
    Applied,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub workspace: String,
    pub session_id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    #[serde(default)]
    pub tool_call_id: String,
    pub source: String,
    pub id: String,
    pub version: String,
    /// Canonical domain reference or sanitized review content. No credentials.
    pub details: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub option_id: String,
    pub feedback: String,
    pub actor: String,
    pub transport: String,
    pub decided_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub options: Vec<Choice>,
    pub schema_version: u32,
    pub id: String,
    pub revision: u64,
    pub kind: Kind,
    /// Absent in a row persisted before the field existed; such a request is
    /// never decided automatically.
    #[serde(default)]
    pub authority: Authority,
    pub scope: Scope,
    /// Trusted passive-delivery origin, such as `chat:telegram:<id>`.
    /// Empty means discoverable through approval lists but not broadcast.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub presentation_origin: String,
    pub subject: Subject,
    pub title: String,
    pub reason: String,
    pub persistent: bool,
    pub blocking: bool,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub state: State,
    pub decision: Option<Decision>,
    pub application: ApplicationState,
    pub application_error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewMetadata>,
}

impl ApprovalMode {
    /// The stable wire, storage and frontend name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    /// Parses [`ApprovalMode::as_str`]; `None` for any other value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(Self::Manual),
            "auto" => Some(Self::Auto),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    /// Whether the policy decides anything without the user.
    pub fn is_automatic(&self) -> bool {
        !matches!(self, Self::Manual)
    }

    /// The frontend label of this mode.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Manual => "MANUAL",
            Self::Auto => "AUTO",
            Self::Yolo => "YOLO",
        }
    }

    /// Whether this policy decides `request` without the user. The single
    /// policy predicate: the service and every backend's committing
    /// transaction ask it, so no other component may decide automatically.
    pub fn decides(&self, request: &Request) -> bool {
        match self {
            Self::Manual => false,
            Self::Auto => request.auto_decidable(),
            Self::Yolo => true,
        }
    }
}

impl Request {
    pub fn is_human_pending(&self) -> bool {
        self.state == State::Pending
            && self
                .review
                .as_ref()
                .is_none_or(|review| review.status == ReviewStatus::HumanRequired)
    }

    /// Whether the session AUTO policy may decide this request without the
    /// user. The conjunction is deliberate: a wrong producer declaration must
    /// not make a plan review or a persistent grant decidable by policy.
    pub fn auto_decidable(&self) -> bool {
        self.authority == Authority::Bounded && self.kind == Kind::Authorization && !self.persistent
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(review) = &self.review {
            review.validate()?;
            if self.state == State::Pending && review.status == ReviewStatus::Completed {
                return Err("pending approval cannot have completed review");
            }
        }
        if self.schema_version != 1
            || self.revision == 0
            || self.id.is_empty()
            || self.scope.workspace.is_empty()
            || self.subject.source.is_empty()
            || self.subject.id.is_empty()
            || self.subject.version.is_empty()
            || self.title.trim().is_empty()
            || self.title.len() > 4096
            || self.reason.len() > 8192
            || self.subject.details.len() > 65536
        {
            return Err("invalid approval request");
        }
        if self.expires_at.is_some_and(|time| time <= self.created_at) {
            return Err("approval expiry must follow creation");
        }
        if self.kind == Kind::Confirmation && self.persistent {
            return Err("confirmation cannot establish persistent authority");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub sequence: i64,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub requests: Vec<Request>,
    pub cursor: i64,
}

/// A confirmation requires a new provider step before later actions can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatchControl {
    #[default]
    Continue,
    Replan,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn settings_preserve_legacy_projection_and_independent_policy() {
        for mode in [ApprovalMode::Manual, ApprovalMode::Auto, ApprovalMode::Yolo] {
            let settings = ApprovalSettings::from_mode(mode);
            assert_eq!(settings.mode(), mode);
            assert_eq!(settings.revision, 1);
            assert_eq!(settings.is_automatic(), mode.is_automatic());
            assert_eq!(
                serde_json::from_str::<ApprovalSettings>(
                    &serde_json::to_string(&settings).unwrap()
                )
                .unwrap(),
                settings
            );
        }
        for reviewer in [
            ApprovalReviewer::User,
            ApprovalReviewer::AutoReview,
            ApprovalReviewer::Bypass,
        ] {
            let settings = ApprovalSettings {
                policy: ApprovalPolicy::Never,
                reviewer,
                revision: 1,
            };
            assert_eq!(settings.mode(), ApprovalMode::Auto);
            assert!(settings.is_automatic());
        }
        assert_eq!(
            ApprovalSettings::default(),
            ApprovalSettings::from_mode(ApprovalMode::Auto)
        );
        assert!(
            ApprovalSettings {
                revision: 0,
                ..ApprovalSettings::default()
            }
            .validate()
            .is_err()
        );
    }

    fn metadata(status: ReviewStatus) -> ReviewMetadata {
        ReviewMetadata {
            status,
            assessment: None,
            settings_revision: 1,
            evidence_digest: "evidence".into(),
            policy_digest: "policy".into(),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn legacy_requests_and_review_states_select_human_pending() {
        let mut request = request(Authority::Boundary, Kind::Authorization, false);
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(!encoded.contains("\"review\""));
        assert!(
            serde_json::from_str::<Request>(&encoded)
                .unwrap()
                .is_human_pending()
        );
        for status in [
            ReviewStatus::Queued,
            ReviewStatus::Reviewing,
            ReviewStatus::Completed,
            ReviewStatus::HumanRequired,
        ] {
            request.review = Some(metadata(status));
            assert_eq!(
                request.is_human_pending(),
                status == ReviewStatus::HumanRequired
            );
        }
        request.state = State::Rejected;
        assert!(!request.is_human_pending());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn review_metadata_rejects_stale_bindings_and_invalid_assessments() {
        let mut review = metadata(ReviewStatus::Queued);
        assert!(review.validate().is_ok());
        assert!(review.matches(1, "evidence", "policy"));
        assert!(!review.matches(2, "evidence", "policy"));
        assert!(!review.matches(1, "changed", "policy"));
        assert!(!review.matches(1, "evidence", "changed"));
        review.status = ReviewStatus::Completed;
        // Human, rule and bypass decisions have no model assessment. Native
        // model decisions independently require an assessment and exact bindings.
        assert!(review.validate().is_ok());
        review.assessment = Some(ReviewAssessment {
            risk: RiskLevel::Low,
            user_authorization: UserAuthorization::High,
            outcome: ReviewOutcome::Allow,
            reason: "Bounded operation".into(),
        });
        assert!(review.validate().is_ok());
        for status in [ReviewStatus::Queued, ReviewStatus::Reviewing] {
            review.status = status;
            assert!(review.validate().is_err());
        }
        review.status = ReviewStatus::Completed;
        assert_eq!(
            serde_json::from_str::<ReviewMetadata>(&serde_json::to_string(&review).unwrap())
                .unwrap(),
            review
        );
        review.assessment.as_mut().unwrap().reason.clear();
        assert!(review.validate().is_err());
        review = metadata(ReviewStatus::Reviewing);
        review.settings_revision = 0;
        assert!(review.validate().is_err());
        review = metadata(ReviewStatus::Queued);
        review.evidence_digest.clear();
        assert!(review.validate().is_err());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn completed_terminal_decisions_need_no_model_assessment() {
        for transport in ["human", "approval-rule", "yolo-mode"] {
            for state in [State::Approved, State::Rejected] {
                let mut request = request(Authority::Boundary, Kind::Authorization, false);
                request.state = state;
                request.review = Some(metadata(ReviewStatus::Completed));
                request.decision = Some(Decision {
                    option_id: if state == State::Approved {
                        "approve"
                    } else {
                        "reject"
                    }
                    .into(),
                    feedback: String::new(),
                    actor: if transport == "human" {
                        "user"
                    } else {
                        "approval-policy"
                    }
                    .into(),
                    transport: transport.into(),
                    decided_at: 2,
                });
                assert!(request.validate().is_ok());
                assert!(!request.is_human_pending());
                let decoded: Request =
                    serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
                assert_eq!(decoded, request);
                assert!(decoded.review.unwrap().assessment.is_none());
            }
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn pending_completed_reviews_fail_persisted_request_validation() {
        let mut request = request(Authority::Boundary, Kind::Authorization, false);
        request.review = Some(metadata(ReviewStatus::Completed));
        for assessment in [
            None,
            Some(ReviewAssessment {
                risk: RiskLevel::Low,
                user_authorization: UserAuthorization::High,
                outcome: ReviewOutcome::Allow,
                reason: "Authorized operation".into(),
            }),
        ] {
            request.review.as_mut().unwrap().assessment = assessment;
            let decoded: Request =
                serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
            assert_eq!(
                decoded.validate(),
                Err("pending approval cannot have completed review")
            );
        }
        request.state = State::Approved;
        request.review = Some(metadata(ReviewStatus::HumanRequired));
        request.review.as_mut().unwrap().assessment = Some(ReviewAssessment {
            risk: RiskLevel::Medium,
            user_authorization: UserAuthorization::Unknown,
            outcome: ReviewOutcome::Ask,
            reason: "Needs a user decision".into(),
        });
        // A later admitted human decision need not replace the model's Ask.
        assert!(request.validate().is_ok());
    }

    fn request(authority: Authority, kind: Kind, persistent: bool) -> Request {
        Request {
            options: Vec::new(),
            schema_version: 1,
            id: "r".into(),
            revision: 1,
            kind,
            authority,
            scope: Scope {
                workspace: "/w".into(),
                session_id: "s".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            presentation_origin: String::new(),
            subject: Subject {
                tool_call_id: String::new(),
                source: "test".into(),
                id: "op".into(),
                version: "1".into(),
                details: "details".into(),
            },
            title: "Title".into(),
            reason: "Reason".into(),
            persistent,
            blocking: true,
            created_at: 1,
            expires_at: None,
            state: State::Pending,
            decision: None,
            application: ApplicationState::NotStarted,
            application_error: String::new(),
            review: None,
        }
    }

    #[test]
    fn only_a_declared_bounded_authorization_is_auto_decidable() {
        assert!(request(Authority::Bounded, Kind::Authorization, false).auto_decidable());
        // A declaration must not make a plan review or a durable grant
        // decidable by the session policy.
        assert!(!request(Authority::Bounded, Kind::Confirmation, false).auto_decidable());
        assert!(!request(Authority::Bounded, Kind::Authorization, true).auto_decidable());
        for authority in [
            Authority::Consent,
            Authority::Boundary,
            Authority::Durable,
            Authority::Unclassified,
        ] {
            assert!(!request(authority, Kind::Authorization, false).auto_decidable());
        }
    }

    #[test]
    fn the_session_mode_decides_exactly_its_own_class() {
        let bounded = request(Authority::Bounded, Kind::Authorization, false);
        let review = request(Authority::Consent, Kind::Confirmation, false);
        let escalation = request(Authority::Boundary, Kind::Authorization, false);
        let grant = request(Authority::Durable, Kind::Authorization, true);
        let legacy = request(Authority::Unclassified, Kind::Authorization, false);
        assert!(!ApprovalMode::Manual.decides(&bounded));
        assert!(!ApprovalMode::Manual.decides(&review));
        assert!(ApprovalMode::Auto.decides(&bounded));
        for request in [&review, &escalation, &grant, &legacy] {
            assert!(!ApprovalMode::Auto.decides(request));
            assert!(ApprovalMode::Yolo.decides(request));
        }
        assert!(ApprovalMode::Yolo.decides(&bounded));
        assert_eq!(
            serde_json::to_string(&ApprovalMode::Yolo).unwrap(),
            "\"yolo\""
        );
        assert_eq!(ApprovalMode::default(), ApprovalMode::Auto);
        for mode in [ApprovalMode::Manual, ApprovalMode::Auto, ApprovalMode::Yolo] {
            assert_eq!(ApprovalMode::parse(mode.as_str()), Some(mode));
            assert_eq!(
                serde_json::to_string(&mode).unwrap(),
                format!("\"{}\"", mode.as_str())
            );
        }
        assert_eq!(ApprovalMode::parse("automatic"), None);
    }

    #[test]
    fn presentation_origin_is_optional_and_round_trips_when_present() {
        let request = request(Authority::Consent, Kind::Confirmation, false);
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(!encoded.contains("presentation_origin"));
        assert_eq!(serde_json::from_str::<Request>(&encoded).unwrap(), request);

        let mut routed = request;
        routed.presentation_origin = "chat:telegram:42".into();
        let encoded = serde_json::to_string(&routed).unwrap();
        assert!(encoded.contains("\"presentation_origin\":\"chat:telegram:42\""));
        assert_eq!(serde_json::from_str::<Request>(&encoded).unwrap(), routed);
    }

    #[test]
    fn a_persisted_request_without_an_authority_class_stays_a_user_decision() {
        let declared = request(Authority::Boundary, Kind::Authorization, false);
        let encoded = serde_json::to_string(&declared).unwrap();
        assert!(encoded.contains("\"authority\":\"boundary\""));
        assert_eq!(serde_json::from_str::<Request>(&encoded).unwrap(), declared);
        let mut legacy: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        legacy.as_object_mut().unwrap().remove("authority");
        let legacy = serde_json::from_str::<Request>(&legacy.to_string()).unwrap();
        assert_eq!(legacy.authority, Authority::Unclassified);
        assert!(!legacy.auto_decidable());
        let mut invalid: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        invalid["authority"] = serde_json::json!("harmless");
        assert!(serde_json::from_str::<Request>(&invalid.to_string()).is_err());
    }
}
