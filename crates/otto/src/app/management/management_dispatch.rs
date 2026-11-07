//! The typed dispatch of the management requests: memory requests keep their
//! failure category for frontends that map it (the HTTP routes); the other
//! requests report text, as `/skill`, `/mcp`, `/timers` and `/tasks` print it.

use super::{ManagementError, ManagementErrorKind, RememberRequest, Reply, Request};
use crate::app::Controller;

impl Controller {
    /// [`Self::management`] with a typed error, for frontends that map the
    /// failure category (the HTTP routes) rather than print it.
    pub fn management_typed(&self, request: Request) -> Result<Reply, ManagementError> {
        use Request::*;
        if self.memory_wiring().external() {
            if let Remember {
                scope,
                kind,
                key,
                text,
                // A plugin store has no review deadline field.
                review_at: _,
            } = request
            {
                return self
                    .remember_with_service(&scope, kind, key, text)
                    .map_err(|message| {
                        ManagementError::new(ManagementErrorKind::Internal, message)
                    });
            }
            if matches!(
                &request,
                MemoryList { .. }
                    | MemoryShow { .. }
                    | MemorySearch { .. }
                    | MemoryForget { .. }
                    | MemoryHistory { .. }
                    | MemoryDiff { .. }
                    | MemoryRestore { .. }
                    | MemoryState { .. }
                    | MemoryVerify { .. }
                    | MemoryPropose { .. }
                    | MemoryReviewList
                    | MemoryCandidatePage { .. }
                    | MemoryReview { .. }
            ) {
                let message = match &self.memory_wiring().plugin {
                    Some(plugin) => format!(
                        "memory source plugin:{} does not support this command; the plugin manages its records",
                        plugin.package
                    ),
                    None => "memory operation is unsupported by the native record management interface; the selected memory service manages its records".into(),
                };
                return Err(ManagementError::new(
                    ManagementErrorKind::Unsupported,
                    message,
                ));
            }
        }
        match request {
            request @ (MemoryList { .. }
            | MemoryShow { .. }
            | MemorySearch { .. }
            | MemoryForget { .. }
            | MemoryHistory { .. }
            | MemoryDiff { .. }
            | MemoryRestore { .. }
            | MemoryState { .. }
            | MemoryVerify { .. }
            | MemoryPropose { .. }
            | MemoryReviewList
            | MemoryCandidatePage { .. }
            | MemoryReview { .. }) => self.memory_request(request),
            Remember {
                scope,
                kind,
                key,
                text,
                review_at,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let scope = match scope.as_str() {
                    "user" => user,
                    "workspace" => workspace,
                    _ => {
                        return Ok(Reply::Usage(format!(
                            "unknown scope {scope:?}, want user or workspace"
                        )));
                    }
                };
                Ok(Reply::Remembered(
                    service
                        .remember(&RememberRequest {
                            scope,
                            kind,
                            key,
                            text,
                            review_at,
                            ..Default::default()
                        })
                        .map_err(ManagementError::from)?,
                ))
            }
            request => self
                .other_request(request)
                .map_err(|message| ManagementError::new(ManagementErrorKind::Internal, message)),
        }
    }
}
