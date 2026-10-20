//! Transport calls, reconnection and snapshot adoption on the attached UI.

use super::*;

impl<'a> Ui<'a> {
    pub(super) fn report(&mut self, what: &str, error: Error) {
        if matches!(error, Error::Unreachable(_)) {
            self.disconnect();
        } else {
            self.app.push_system(format!("{what}: {error}"));
        }
    }

    pub(super) fn disconnect(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false;
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.mutation)
        {
            self.pending = None;
        }
        self.followed = None;
        self.status = None;
        self.compacting = None;
        self.rows.clear();
        self.last_row = None;
        self.app.finish_turn(false, true);
        self.app.connected = false;
        self.app.push_system(DISCONNECTED);
        self.transcript = self.transcript.wrapping_add(1);
    }

    pub(super) fn pending_call(&mut self, mutation: bool, call: TransportCall<'a>) {
        self.pending = Some(PendingTransport {
            session: self.remote.session(),
            epoch: self.epoch,
            transcript: self.transcript,
            mutation,
            call,
        });
    }

    /// Reconnect commits nothing until the same session has been authoritatively reloaded.
    pub(super) fn reconnect(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let remote = self.remote;
        let session = remote.session();
        self.pending_call(
            false,
            Box::pin(async move {
                let status = within(remote.client.status()).await?;
                let id = within(remote.client.open_session(None, Some(&session))).await?;
                if id != session {
                    return Err(Error::Unreachable("reconnect changed session".into()));
                }
                let (info, history, tasks) = tokio::try_join!(
                    within(remote.client.session(&session)),
                    within(remote.client.history(&session, None)),
                    within(remote.client.session_tasks(&session)),
                )?;
                Ok(TransportResult::Reconnect {
                    snapshot: Snapshot {
                        info: Some(info),
                        tasks: Some(tasks),
                        history: Some(history),
                        stream: None,
                        done: None,
                        row: None,
                    },
                    status,
                })
            }),
        );
    }

    /// Coalesces status changes while a snapshot or admitted operation is pending.
    pub(super) fn sync(&mut self) {
        if !self.connected || self.pending.is_some() {
            return;
        }
        let session = self.remote.session();
        let row = self.rows.iter().find(|row| row.id == session).cloned();
        let decision = row.as_ref().and_then(|row| {
            follow_decision(
                row,
                self.followed.as_ref().map(|f| f.stream.turn_id.as_str()),
                &self.remote.own(),
                self.last_done.as_deref(),
            )
        });
        if !self.needs_sync && self.last_row == row && decision.is_none() {
            return;
        }
        let fresh = self.fresh;
        let remote = self.remote;
        self.needs_sync = false;
        self.pending_call(
            false,
            Box::pin(async move {
                let (before, done, follow) = match decision {
                    Some(Follow::Own(id)) => (None, None, Some(id)),
                    Some(Follow::Foreign(id)) => (Some(id.clone()), None, Some(id)),
                    Some(Follow::Reload(id)) => (None, Some(id), None),
                    None => (None, None, None),
                };
                let reload = before.is_some() || done.is_some() && !fresh;
                let history = async {
                    if reload {
                        within(remote.client.history(&session, before.as_deref()))
                            .await
                            .map(Some)
                    } else {
                        Ok(None)
                    }
                };
                let following = follow.is_some();
                let stream = async {
                    match follow {
                        Some(id) => within(remote.client.turn_events(&session, &id, None))
                            .await
                            .map(Some),
                        None => Ok(None),
                    }
                };
                let metadata = async {
                    if following {
                        return Ok((None, None));
                    }
                    let tasks = async {
                        match within(remote.client.session_tasks(&session)).await {
                            Ok(tasks) => Ok(Some(tasks)),
                            Err(Error::Http { .. }) => Ok(None),
                            Err(error) => Err(error),
                        }
                    };
                    let (info, tasks) =
                        tokio::try_join!(within(remote.client.session(&session)), tasks)?;
                    Ok::<_, Error>((Some(info), tasks))
                };
                let ((info, tasks), history, stream) = tokio::try_join!(metadata, history, stream)?;
                Ok(TransportResult::Sync(Snapshot {
                    info,
                    tasks,
                    history,
                    stream,
                    done,
                    row,
                }))
            }),
        );
    }

    pub(super) fn on_transport(&mut self, result: Result<TransportResult, Error>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.session != self.remote.session() || pending.epoch != self.epoch {
            self.cancel_after_send = false;
            return;
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.cancel_after_send = false;
                if pending.mutation && matches!(error, Error::Unreachable(_)) {
                    self.app.push_system(format!(
                        "{error}; outcome unknown, reconcile before retrying"
                    ));
                }
                if !self.connected {
                    return;
                }
                if !pending.mutation && !matches!(error, Error::Unreachable(_)) {
                    self.last_row = self
                        .rows
                        .iter()
                        .find(|row| row.id == pending.session)
                        .cloned();
                    self.last_done = self.last_row.as_ref().and_then(|row| row.turn_id.clone());
                }
                if self.followed.is_none() && self.compacting.is_none() && self.app.busy() {
                    self.app.finish_turn(false, false);
                }
                self.report("serve", error);
                return;
            }
        };
        match result {
            TransportResult::Sync(snapshot) => {
                if snapshot.history.is_some() && pending.transcript != self.transcript {
                    self.needs_sync = true;
                    return;
                }
                self.commit_snapshot(snapshot);
            }
            TransportResult::Reconnect { snapshot, status } => {
                if pending.transcript != self.transcript {
                    return;
                }
                self.commit_snapshot(snapshot);
                self.status = Some(status);
                self.connected = true;
                self.app.connected = true;
                self.app.push_system("reconnected to otto serve");
                self.transcript = self.transcript.wrapping_add(1);
                self.app.tick_features();
                self.app.request_approvals();
                self.last_done = None;
                self.fresh = true;
            }
            TransportResult::Prompt { reply, stream } => {
                let sent_turn = stream.as_ref().map(|stream| stream.turn_id.clone());
                if let Some(reply) = reply {
                    self.app.push_system(reply);
                    self.transcript = self.transcript.wrapping_add(1);
                }
                if let Some(stream) = stream {
                    let turn = stream.turn_id.clone();
                    self.remote.state().own.push(turn);
                    if self.connected && self.followed.is_none() {
                        self.adopt_stream(stream);
                    }
                } else if self.followed.is_none() && self.compacting.is_none() {
                    self.app.finish_turn(false, true);
                }
                if self.cancel_after_send {
                    self.cancel_after_send = false;
                    if let Some(turn) = self
                        .followed
                        .as_ref()
                        .map(|followed| followed.stream.turn_id.clone())
                        .or(sent_turn)
                    {
                        self.cancel_turn(turn);
                    }
                }
            }
            TransportResult::Cancel => {
                self.cancel_after_send = false;
            }
        }
    }

    fn commit_snapshot(&mut self, snapshot: Snapshot) {
        let reload = snapshot.history.is_some();
        {
            let mut state = self.remote.state();
            if let Some(info) = snapshot.info {
                state.info = info;
            }
            if let Some(history) = snapshot.history {
                state.history = history;
            }
        }
        if reload {
            self.app.refresh(&Backend::Attach(self.remote));
            self.transcript = self.transcript.wrapping_add(1);
        } else {
            self.app.refresh_info(&Backend::Attach(self.remote));
        }
        if let Some(tasks) = snapshot.tasks {
            self.app.tasks = tasks;
        }
        self.last_row = snapshot.row;
        self.fresh = false;
        if let Some(id) = snapshot.done {
            self.remote.finish(&id);
            self.last_done = Some(id);
        }
        if let Some(stream) = snapshot.stream {
            self.adopt_stream(stream);
            self.needs_sync = true;
        }
    }

    pub(super) fn adopt_stream(&mut self, stream: TurnStream) {
        self.remote.state().running = Some(stream.turn_id.clone());
        self.followed = Some(Followed {
            stream,
            error_rendered: false,
        });
        self.app.start_turn();
    }

    pub(super) async fn drain_transport(&mut self) {
        if let Some(pending) = self.pending.as_mut()
            && pending.mutation
        {
            let result = pending.call.as_mut().await;
            self.on_transport(result);
        }
    }
}
