//! Portable approval records. Original envelopes retain unknown additive metadata.
use crate::storage::migration::{Error, Result};
use serde::{Deserialize, Serialize};
fn json_error(_: serde_json::Error) -> Error {
    Error::Invalid("invalid approval record encoding".into())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalRecord {
    Mode {
        workspace: String,
        session_id: String,
        automatic: bool,
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
