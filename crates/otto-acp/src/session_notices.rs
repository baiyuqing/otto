//! Connection memory requests, session listing and notice delivery.

use super::*;

impl Connection {
    pub(super) fn session_notice(
        &self,
        session_id: &str,
        notice_id: u64,
        text: &str,
        presentation_origin: &str,
    ) {
        let meta = serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({
                "sessionNotice": true,
                "noticeId": notice_id,
                "presentationOrigin": presentation_origin,
            }),
        )]);
        self.update(
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text))).meta(meta),
            ),
        );
    }

    /// Memory review. A local session uses the controller it owns; an attached
    /// session forwards to serve's session memory routes. Either way only a
    /// session this connection opened is reviewed.
    pub(super) async fn memory_request(&self, method: &str, params: Value) -> Reply {
        let pending = method == memory::PENDING_METHOD;
        match &self.backend {
            Backend::Local(local) => {
                let controller = |id: &str| {
                    local
                        .session(id)
                        .map(|session| Arc::clone(&session.controller))
                        .ok_or_else(unknown_session)
                };
                if pending {
                    let params: memory::PendingParams = serde_json::from_value(params)
                        .map_err(|failure| invalid_params(failure.to_string()))?;
                    memory::pending(controller(&params.session_id)?.as_ref(), &params)
                } else {
                    let params: memory::ReviewParams = serde_json::from_value(params)
                        .map_err(|failure| invalid_params(failure.to_string()))?;
                    memory::review(controller(&params.session_id)?.as_ref(), &params)
                }
            }
            Backend::Attach(relay) => {
                let result = if pending {
                    let params: memory::PendingParams = serde_json::from_value(params)
                        .map_err(|failure| invalid_params(failure.to_string()))?;
                    if relay.slot(&params.session_id).is_none() {
                        return Err(unknown_session());
                    }
                    memory::attached_pending(&relay.client, &params).await
                } else {
                    let params: memory::ReviewParams = serde_json::from_value(params)
                        .map_err(|failure| invalid_params(failure.to_string()))?;
                    if relay.slot(&params.session_id).is_none() {
                        return Err(unknown_session());
                    }
                    memory::attached_review(&relay.client, &params).await
                };
                result.map_err(|failure| match failure {
                    memory::AttachedFailure::Reply(reply) => reply,
                    memory::AttachedFailure::Unreachable(failure) => relay.fail(failure),
                })
            }
        }
    }

    pub(super) async fn list_sessions(&self, params: Value) -> Reply {
        let workspace = match params.get("cwd") {
            None | Some(Value::Null) => self.workspace.clone(),
            Some(value) => self.request_workspace(value.as_str())?,
        };
        let sessions = match &self.backend {
            Backend::Local(local) => local.list_sessions().await?,
            Backend::Attach(relay) => relay.list_sessions(&workspace).await?,
        };
        result(ListSessionsResponse::new(sessions))
    }

    /// Sends the command list after a successful `session/new` or
    /// `session/load` reply, so the client already knows the session id.
    pub(super) fn advertise_commands(&self, reply: &Reply) {
        if let Some(id) = reply
            .as_ref()
            .ok()
            .and_then(|value| value.get("sessionId"))
            .and_then(Value::as_str)
        {
            self.update(id, command::available_commands());
        }
    }

    pub(super) fn notices_ready(&self, reply: &Reply) {
        if let Some(id) = reply
            .as_ref()
            .ok()
            .and_then(|value| value.get("sessionId"))
            .and_then(Value::as_str)
            && let Some(ready) = self
                .notice_ready
                .lock()
                .expect("notice ready map")
                .remove(id)
        {
            let _ = ready.send(true);
        }
    }

    pub(super) fn say(&self, session_id: &str, text: &str) {
        self.update(
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        );
    }

    pub(super) fn wake_reply(&self, session_id: &str, reply_id: u64, text: &str) {
        let meta = serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({"wakeReply": true, "replyId": reply_id}),
        )]);
        self.update(
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text))).meta(meta),
            ),
        );
    }
}

impl Connection {
    pub(super) fn local_command_receipts(
        &self,
        session_id: &str,
        receipts: &[otto_core::session::local_command::LocalCommandReceipt],
    ) {
        self.send(
            json!({"jsonrpc":"2.0", "method":"_otto/local_commands/update",
            "params":{"sessionId":session_id, "receipts":receipts}}),
        );
    }
}
