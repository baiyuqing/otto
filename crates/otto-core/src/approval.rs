//! Frontend-neutral human approval contracts. Native callers supply identity,
//! time, storage and domain application; models never supply decision authority.
use serde::{Deserialize, Serialize};

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
    /// Whether the session AUTO policy may decide this request without the
    /// user. The conjunction is deliberate: a wrong producer declaration must
    /// not make a plan review or a persistent grant decidable by policy.
    pub fn auto_decidable(&self) -> bool {
        self.authority == Authority::Bounded && self.kind == Kind::Authorization && !self.persistent
    }

    pub fn validate(&self) -> Result<(), &'static str> {
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
