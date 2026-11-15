//! Connection session creation, restore, wake loop and notice watcher start.

use super::*;

impl Connection {
    /// The prompt slot of a session this connection has opened.
    /// The controller of a session this process runs itself.
    pub(super) fn local_controller(&self, id: &str) -> Option<Arc<Controller>> {
        match &self.backend {
            Backend::Local(local) => local
                .session(id)
                .map(|session| Arc::clone(&session.controller)),
            Backend::Attach(_) => None,
        }
    }

    /// Restores the `/queue` prompts saved before Otto stopped, after the
    /// session's pending wake turn (the interrupted turn's recovery) has run:
    /// they wait until the slot is free and either no notification is
    /// pending or a wake turn has ended since. The first of them delivers
    /// the restore notice.
    pub(super) fn restore_queued_prompts(self: &Arc<Self>, session_id: &str) {
        let (Some(controller), Some(slot)) =
            (self.local_controller(session_id), self.slot(session_id))
        else {
            return;
        };
        let prompts = controller.restorable_prompts();
        if prompts.is_empty() {
            return;
        }
        let connection = Arc::clone(self);
        let session_id = session_id.to_string();
        let stop = self.task_stop.clone();
        tokio::spawn(async move {
            let mut released = slot.released.subscribe();
            let start = *released.borrow_and_update();
            loop {
                let idle = {
                    let state = slot.state.lock().expect("prompt slot");
                    state.is_idle()
                };
                let pending = controller
                    .notification_mailbox()
                    .is_some_and(|mailbox| !mailbox.is_empty());
                if idle && (!pending || *released.borrow() != start) {
                    break;
                }
                tokio::select! {
                    () = stop.cancelled() => return,
                    changed = released.changed() => if changed.is_err() { return },
                    () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                }
            }
            let mut start_worker = false;
            for prompt in &prompts {
                if let Ok(start) = slot.enqueue_with_provenance(
                    prompt.text.clone(),
                    (prompt.channel, prompt.origin.clone()),
                    Some(prompt.input_id.clone()),
                    prompt.input_provenance,
                ) {
                    start_worker |= start;
                }
            }
            controller.announce_restored_prompts(&prompts);
            if start_worker {
                slot.drain_queue(connection, session_id).await;
            }
        });
    }

    pub(super) fn slot(&self, id: &str) -> Option<Arc<PromptSlot>> {
        match &self.backend {
            Backend::Local(local) => local.session(id).map(|session| Arc::clone(&session.prompt)),
            Backend::Attach(relay) => relay.slot(id),
        }
    }

    /// Local ACP stays pinned to its workspace; serve owns attach admission.
    pub(super) fn request_workspace(&self, cwd: Option<&str>) -> Result<PathBuf, Error> {
        let cwd = cwd.ok_or_else(|| invalid_params("cwd is required"))?;
        if !Path::new(cwd).is_absolute() {
            return Err(invalid_params("cwd must be an absolute path"));
        }
        if matches!(&self.backend, Backend::Attach(_)) {
            return Ok(PathBuf::from(cwd));
        }
        match otto::fsops::canonical_directory(Path::new(cwd)) {
            Ok(canonical) if canonical == self.workspace => Ok(canonical),
            _ => Err(invalid_params(format!(
                "cwd must be the workspace {}",
                self.workspace.display()
            ))),
        }
    }

    pub(super) async fn new_session(self: &Arc<Self>, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        let workspace = self.request_workspace(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        let reply = match &self.backend {
            Backend::Local(local) => local.new_session().await,
            Backend::Attach(relay) => relay.new_session(&workspace).await,
        }?;
        let id = reply
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.start_wake_loop(id);
        self.start_notice_watcher(id).await;
        Ok(reply)
    }

    pub(super) async fn load_session(self: &Arc<Self>, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        let workspace = self.request_workspace(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        let id = params.session_id.unwrap_or_default();
        if !is_session_id(&id) {
            return Err(invalid_params(
                "sessionId must be 32 lowercase hexadecimal characters",
            ));
        }
        let (history, plan) = match &self.backend {
            Backend::Local(local) => local.load_session(&id, &workspace).await?,
            Backend::Attach(relay) => relay.load_session(&id, &workspace).await?,
        };
        for update in update::history_updates(&history) {
            self.update(&id, update);
        }
        let receipts = match &self.backend {
            Backend::Local(local) => local
                .session(&id)
                .map(|session| session.controller.info().local_commands)
                .unwrap_or_default(),
            Backend::Attach(relay) => relay
                .client
                .session(&id)
                .await
                .map(|info| info.local_commands)
                .unwrap_or_default(),
        };
        self.local_command_receipts(&id, &receipts);
        self.restore_queued_prompts(&id);
        if !plan.is_empty() {
            self.update(&id, update::plan_update(&plan));
        }
        self.start_wake_loop(&id);
        self.start_notice_watcher(&id).await;
        Ok(json!({ "sessionId": id }))
    }

    /// Runs the session's wake turns until the connection ends: a due
    /// sub-agent or timer notification starts a turn whenever nothing else
    /// holds the session. The turn streams nothing; its reply text is queued
    /// on the controller and sent by the wake-reply watcher.
    pub(super) fn start_wake_loop(&self, session_id: &str) {
        let Backend::Local(local) = &self.backend else {
            return;
        };
        let Some(session) = local.session(session_id) else {
            return;
        };
        let Some(mailbox) = session.controller.notification_mailbox() else {
            return;
        };
        let mut loops = self.wake_loops.lock().expect("wake loops");
        if loops.contains_key(session_id) {
            return;
        }
        // Registered after the check: a mailbox listener is never removed.
        let mut updates = otto::runtime::workflow_notifications::updates(mailbox.as_ref());
        let stop = self.task_stop.clone();
        loops.insert(
            session_id.to_string(),
            tokio::spawn(async move {
                let mut released = session.prompt.released.subscribe();
                loop {
                    if wake_turn(&session, &stop).await {
                        // A failed turn leaves its notification queued; wait
                        // before retrying so a persistent error does not spin.
                        tokio::select! {
                            () = stop.cancelled() => return,
                            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        }
                    }
                    tokio::select! {
                        biased;
                        () = stop.cancelled() => return,
                        changed = updates.changed() => if changed.is_err() { return },
                        changed = released.changed() => if changed.is_err() { return },
                    }
                }
            }),
        );
    }

    /// Follows the session's notices until the connection ends. A local
    /// session starts at the beginning, which includes the notices queued when
    /// its controller was built. An attached session starts at the current
    /// cursor, so a reconnect does not replay old outcomes.
    pub(super) async fn start_notice_watcher(self: &Arc<Self>, session_id: &str) {
        let id = session_id.to_string();
        let controller = match &self.backend {
            Backend::Local(local) => local
                .session(&id)
                .map(|session| Arc::clone(&session.controller)),
            // An attached session's controller lives in serve; its notices are
            // polled over the socket below.
            Backend::Attach(_) => None,
        };
        let connection = Arc::clone(self);
        let stop = self.notice_stop.child_token();
        {
            let mut watchers = self.watching_notices.lock().expect("notice watchers");
            if let Some(current) = &controller
                && watchers
                    .get(&id)
                    .and_then(|watch| watch.controller.upgrade())
                    .is_some_and(|watched| Arc::ptr_eq(&watched, current))
            {
                return;
            }
            // A different client may have closed and reopened this session id.
            if let Some(previous) = watchers.insert(
                id.clone(),
                NoticeWatch {
                    controller: controller.as_ref().map(Arc::downgrade).unwrap_or_default(),
                    stop: stop.clone(),
                },
            ) {
                previous.stop.cancel();
            }
        }
        let (ready, start) = tokio::sync::watch::channel(false);
        self.notice_ready
            .lock()
            .expect("notice ready map")
            .insert(id.clone(), ready);
        match &self.backend {
            Backend::Local(_) => {
                let Some(controller) = controller else {
                    return;
                };
                let changed = controller.notices_changed();
                // This session owns the controller, so every notice it holds,
                // including one queued when it was built, is new to this client.
                let replies_changed = controller.wake_replies_changed();
                let (wake_connection, wake_controller) =
                    (Arc::clone(&connection), Arc::clone(&controller));
                let (wake_id, wake_stop, wake_start) = (id.clone(), stop.clone(), start.clone());
                tokio::spawn(async move {
                    if !notice_watcher_ready(wake_start, &wake_stop).await {
                        return;
                    }
                    watch_controller_notices(
                        wake_connection,
                        wake_controller,
                        wake_id,
                        wake_stop,
                        replies_changed,
                        0,
                        true,
                    )
                    .await;
                });
                tokio::spawn(async move {
                    if !notice_watcher_ready(start, &stop).await {
                        return;
                    }
                    watch_controller_notices(connection, controller, id, stop, changed, 0, false)
                        .await;
                });
            }
            Backend::Attach(relay) => {
                // An older serve has no wake-replies route; only this polling
                // stops then.
                let mut replies_after = tokio::select! {
                    biased;
                    () = stop.cancelled() => return,
                    result = relay.client.wake_replies(&id, 0) => match result {
                        Ok(initial) => Some(initial.last),
                        Err(otto::client::Error::Http { status: 404, .. }) => None,
                        Err(failure) => {
                            relay.fail(failure);
                            return;
                        }
                    },
                };
                let connection = Arc::clone(&connection);
                tokio::spawn(async move {
                    if !notice_watcher_ready(start, &stop).await {
                        return;
                    }
                    loop {
                        tokio::select! {
                            biased;
                            () = stop.cancelled() => return,
                            () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        }
                        if let Some(replies_cursor) = replies_after {
                            let polled = match &connection.backend {
                                Backend::Attach(relay) => tokio::select! {
                                    biased;
                                    () = stop.cancelled() => return,
                                    result = relay.client.wake_replies(&id, replies_cursor) => result,
                                },
                                Backend::Local(_) => return,
                            };
                            match polled {
                                Ok(page) => {
                                    emit_notices(
                                        &connection,
                                        &id,
                                        page.notices.into_iter().map(|n| (n.id, n.text)).collect(),
                                        true,
                                    );
                                    replies_after = Some(page.last);
                                }
                                Err(otto::client::Error::Http { status: 404, .. }) => {
                                    replies_after = None;
                                }
                                Err(failure) => {
                                    if let Backend::Attach(relay) = &connection.backend {
                                        relay.fail(failure);
                                    }
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        }
    }
}
