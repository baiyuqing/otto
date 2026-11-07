//! Human-only local command history. These records are deliberately not messages.
use serde::{Deserialize, Serialize};

pub const LOCAL_COMMAND_CUSTOM_TYPE: &str = "otto.local_command";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalCommandStatus {
    Pending,
    Success,
    Error,
}

/// An append-only observation of one invocation. Later observations with the
/// same id replace its display state, never its persisted history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCommandReceipt {
    pub id: String,
    pub command: String,
    pub parameters: String,
    pub status: LocalCommandStatus,
    pub summary: String,
    pub output: String,
    pub created_at: String,
    /// Number of conversation messages preceding this invocation.
    pub after_messages: usize,
}

impl LocalCommandReceipt {
    pub fn marker(&self) -> &'static str {
        match self.status {
            LocalCommandStatus::Pending => "!",
            LocalCommandStatus::Success => "✓",
            LocalCommandStatus::Error => "✗",
        }
    }
}

/// Fold observations by invocation id without executing any command.
pub fn fold_local_commands(
    receipts: impl IntoIterator<Item = LocalCommandReceipt>,
) -> Vec<LocalCommandReceipt> {
    let mut folded: Vec<LocalCommandReceipt> = Vec::new();
    for receipt in receipts {
        if let Some(previous) = folded.iter_mut().find(|previous| previous.id == receipt.id) {
            *previous = receipt;
        } else {
            folded.push(receipt);
        }
    }
    folded
}
