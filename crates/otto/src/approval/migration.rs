//! Portable approval records. Original envelopes retain unknown additive metadata.
use crate::storage::migration::{Error, Result};
use serde::{Deserialize, Serialize};
fn json_error(_: serde_json::Error) -> Error {
    Error::Invalid("invalid approval record encoding".into())
}

/// The stored mode of one session. `automatic` is the pre-mode form of the
/// same preference: `false` was MANUAL and `true` was AUTO. It is accepted on
/// decode only, so an interrupted cross-backend migration that exported a
/// boolean payload still imports with its meaning, and the mode form is what
/// every new export writes.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredMode {
    Automatic { automatic: bool },
    Mode { mode: crate::approval::ApprovalMode },
}

impl From<StoredMode> for crate::approval::ApprovalMode {
    fn from(stored: StoredMode) -> Self {
        match stored {
            StoredMode::Automatic { automatic: true } => Self::Auto,
            StoredMode::Automatic { automatic: false } => Self::Manual,
            StoredMode::Mode { mode } => mode,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalRecord {
    Mode {
        workspace: String,
        session_id: String,
        /// Written as the mode; decoded from either form.
        mode: StoredMode,
    },
    Request {
        subject_key: String,
        request: String,
    },
    Visibility {
        approval_id: String,
        workspace: String,
        session_id: String,
    },
    InterruptedClaim {
        approval_id: String,
        recovery: String,
    },
    Event {
        sequence: i64,
        request: String,
    },
}
impl ApprovalRecord {
    /// The mode record of one session, in the current export form.
    pub fn mode(
        workspace: String,
        session_id: String,
        mode: crate::approval::ApprovalMode,
    ) -> Self {
        Self::Mode {
            workspace,
            session_id,
            mode: StoredMode::Mode { mode },
        }
    }
    pub fn metadata(&self) -> Result<(String, String)> {
        Ok(match self {
            ApprovalRecord::Mode {
                workspace,
                session_id,
                ..
            } => (
                format!(
                    "mode:{}",
                    serde_json::to_string(&(workspace, session_id)).map_err(json_error)?
                ),
                "1".into(),
            ),
            ApprovalRecord::Request { request, .. } => {
                let typed = crate::approval::codec::decode_persisted(request)
                    .map_err(|_| Error::Invalid("invalid approval authority".into()))?;
                (format!("request:{}", typed.id), typed.revision.to_string())
            }
            ApprovalRecord::Visibility {
                approval_id,
                workspace,
                session_id,
            } => (
                format!(
                    "visibility:{}",
                    serde_json::to_string(&(approval_id, workspace, session_id))
                        .map_err(json_error)?
                ),
                "1".into(),
            ),
            ApprovalRecord::InterruptedClaim { approval_id, .. } => {
                (format!("claim:{approval_id}"), "1".into())
            }
            ApprovalRecord::Event { sequence, request } => (
                format!("event:{sequence}"),
                crate::approval::codec::decode_persisted(request)
                    .map_err(|_| Error::Invalid("invalid approval authority".into()))?
                    .revision
                    .to_string(),
            ),
        })
    }
}
