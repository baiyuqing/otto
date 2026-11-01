//! Local-controller sessions and the controller notice watchers.

use super::*;

/// Runs one wake turn when a notification is pending and nothing else holds
/// the session. Returns true when the turn failed.
pub(super) async fn wake_turn(session: &Session, stop: &CancellationToken) -> bool {
    // An empty check must not release the slot: that release would wake this
    // same loop again even though no notification arrived.
    if session
        .controller
        .notification_mailbox()
        .is_none_or(|mailbox| mailbox.is_empty())
    {
        return false;
    }
    let Some(token) = session.prompt.begin_wake(stop) else {
        return false;
    };
    let controller = &session.controller;
    let failure = match controller.prepare_wake() {
        Ok(None) => None,
        Ok(Some(wake)) => controller
            .run_with_channel(
                Step::Wake(wake),
                None,
                None,
                &mut |_| {},
                &token,
                |_| async { ApprovalDecision::Deny },
            )
            .await
            .err()
            .map(|failure| failure.to_string()),
        Err(message) if message == PROMPT_ACTIVE || message == CLOSED => None,
        Err(message) => Some(message),
    };
    session.prompt.end();
    if let Some(message) = &failure {
        eprintln!(
            "acp: wake turn failed: {}",
            controller.redact_error(message)
        );
    }
    failure.is_some()
}

pub(super) async fn watch_controller_notices(
    connection: Arc<Connection>,
    controller: Arc<Controller>,
    session_id: String,
    stop: CancellationToken,
    mut changed: tokio::sync::watch::Receiver<u64>,
    mut after: u64,
    wake_replies: bool,
) {
    // A receiver subscribed before a line was queued does not fire for it
    // again, so read what is already queued before the first wait.
    loop {
        if stop.is_cancelled() {
            return;
        }
        let last = if wake_replies {
            let (notices, last) = controller.wake_replies_since(after);
            for (reply_id, text) in notices {
                connection.wake_reply(&session_id, reply_id, &text);
            }
            last
        } else {
            let (notices, last) = controller.routed_notices_since(after);
            for notice in notices {
                connection.session_notice(
                    &session_id,
                    notice.id,
                    &notice.text,
                    &notice.presentation_origin,
                );
            }
            last
        };
        after = last;
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            result = changed.changed() => if result.is_err() { return },
        }
    }
}

pub(super) async fn notice_watcher_ready(
    mut start: tokio::sync::watch::Receiver<bool>,
    stop: &CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        () = stop.cancelled() => false,
        result = start.wait_for(|ready| *ready) => result.is_ok(),
    }
}

pub(super) fn emit_notices(
    connection: &Connection,
    session_id: &str,
    notices: Vec<(u64, String)>,
    wake_replies: bool,
) {
    for (notice_id, text) in notices {
        if wake_replies {
            connection.wake_reply(session_id, notice_id, &text);
        } else {
            connection.session_notice(session_id, notice_id, &text, "");
        }
    }
}

impl Local {
    pub(super) fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().expect("session map").get(id).cloned()
    }

    pub(super) fn redact(&self, message: &str) -> String {
        self.config
            .builder
            .redact_error(message, Some(&self.config.runtime))
    }

    pub(super) fn wire(&self, controller: Controller) -> Controller {
        match &self.config.sandbox {
            Some(control) => controller.with_sandbox_control(Arc::clone(control)),
            None => controller,
        }
    }

    pub(super) fn register(&self, controller: Controller) -> (String, Arc<Session>) {
        let controller = Arc::new(self.wire(controller));
        let id = controller.info().session_id;
        let session = Arc::new(Session {
            controller,
            prompt: Arc::default(),
        });
        self.sessions
            .lock()
            .expect("session map")
            .insert(id.clone(), Arc::clone(&session));
        (id, session)
    }

    pub(super) async fn new_session(&self) -> Reply {
        let controller = Controller::create(Arc::clone(&self.config.builder), &self.config.runtime)
            .await
            .map_err(|message| error(INTERNAL_ERROR, self.redact(&message)))?;
        let (id, _) = self.register(controller);
        result(NewSessionResponse::new(id))
    }

    /// Loads the session if this process has not, and returns its history.
    /// Opens the session and returns its history and plan for replay.
    pub(super) async fn load_session(
        &self,
        id: &str,
        workspace: &Path,
    ) -> Result<(Vec<Message>, Vec<otto_core::session::PlanStep>), Error> {
        let session = match self.session(id) {
            Some(session) => session,
            None => {
                let path = session_directory(
                    &self.config.builder.session_root,
                    &workspace.to_string_lossy(),
                )
                .map_err(|error_| error(INTERNAL_ERROR, self.redact(&error_.to_string())))?
                .join(format!("{id}.jsonl"));
                if !path.is_file() {
                    return Err(unknown_session());
                }
                let (controller, warnings) =
                    Controller::open(Arc::clone(&self.config.builder), &path)
                        .await
                        .map_err(|message| error(INTERNAL_ERROR, self.redact(&message)))?;
                for warning in warnings {
                    eprintln!("warning: {}", self.redact(&warning));
                }
                self.register(controller).1
            }
        };
        Ok((session.controller.history(), session.controller.info().plan))
    }

    pub(super) async fn list_sessions(&self) -> Result<Vec<SessionInfo>, Error> {
        let root = self.config.builder.session_root.clone();
        let workspace = self.config.workspace.to_string_lossy().into_owned();
        let listed = if root.exists() {
            tokio::task::spawn_blocking(move || {
                otto::session::list(&root, &workspace, "", MAX_LIST_SESSIONS)
            })
            .await
            .map_err(|join| error(INTERNAL_ERROR, join.to_string()))?
            .map_err(|failure| error(INTERNAL_ERROR, self.redact(&failure.to_string())))?
            .sessions
        } else {
            Vec::new()
        };
        Ok(listed
            .into_iter()
            .map(|info| {
                let title = if info.name.is_empty() {
                    &info.last_user_text
                } else {
                    &info.name
                };
                SessionInfo::new(info.id.clone(), PathBuf::from(&info.cwd))
                    .title(title.chars().take(TITLE_CHARS).collect::<String>())
                    .updated_at(info.modified.to_rfc3339())
            })
            .collect())
    }
}
