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

    /// Memory review uses a controller owned by this process, either a local
    /// ACP session or a serve session opened by this TCP connection.
    pub(super) fn memory_request(&self, method: &str, params: Value) -> Reply {
        if matches!(&self.backend, Backend::Attach(relay) if relay.server.is_none()) {
            return Err(error(METHOD_NOT_FOUND, format!("unknown method {method}")));
        }
        let session_of = |id: &str| {
            match &self.backend {
                Backend::Local(local) => local
                    .session(id)
                    .map(|session| Arc::clone(&session.controller)),
                Backend::Attach(relay) => relay
                    .server
                    .as_ref()
                    .filter(|_| relay.slot(id).is_some())
                    .and_then(|server| server.controller(id)),
            }
            .ok_or_else(unknown_session)
        };
        if method == memory::PENDING_METHOD {
            let params: memory::PendingParams = serde_json::from_value(params)
                .map_err(|failure| invalid_params(failure.to_string()))?;
            memory::pending(session_of(&params.session_id)?.as_ref(), &params)
        } else {
            let params: memory::ReviewParams = serde_json::from_value(params)
                .map_err(|failure| invalid_params(failure.to_string()))?;
            memory::review(session_of(&params.session_id)?.as_ref(), &params)
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
