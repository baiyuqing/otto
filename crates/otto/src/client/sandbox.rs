//! Sandbox configuration changes.

use super::*;

/// The result of one `POST /v1/sessions/{id}/sandbox/changes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SandboxChangeOutcome {
    /// The sandbox summary line after the reload (`SandboxInfo::summary`).
    pub sandbox_summary: String,
    /// The description of the change itself, such as `network allow`.
    pub summary: String,
    /// The configuration file the serve process wrote the change to.
    pub config_path: String,
    /// The prompt that makes the model retry a command the sandbox refused;
    /// `None` when the change does not widen the sandbox.
    pub followup: Option<String>,
}

impl Client {
    /// `POST /v1/sessions/{id}/sandbox/changes`. `change` is
    /// `allow_read_path`, `network` or `exclude_command`.
    pub(crate) async fn sandbox_change(
        &self,
        session_id: &str,
        change: &str,
        value: &str,
    ) -> Result<SandboxChangeOutcome, Error> {
        #[derive(Deserialize)]
        struct Changed {
            #[serde(default)]
            summary: String,
            #[serde(default)]
            change_summary: String,
            #[serde(default)]
            config_path: String,
            #[serde(default)]
            followup: Option<String>,
        }
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/sandbox/changes"),
                &json!({ "change": change, "value": value }),
            ))
            .await?;
        let changed: Changed = decode(response).await?;
        Ok(SandboxChangeOutcome {
            sandbox_summary: changed.summary,
            summary: changed.change_summary,
            config_path: changed.config_path,
            followup: changed.followup,
        })
    }
}
