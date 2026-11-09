//! Open-session registry: lookup, eviction, idle sweeping and resume.

use super::*;

impl Server {
    // ---- registry ----

    /// Shared controller for an already-open session; used by ACP memory review.
    pub fn controller(&self, id: &str) -> Option<Arc<Controller>> {
        self.lookup(id).map(|session| Arc::clone(&session.ctrl))
    }

    /// Shared controller for a route that names a session in a query or a body
    /// rather than in its path, re-opening one the sweeper evicted. `None` when
    /// no such session exists, which the caller answers 404 for.
    pub(crate) async fn controller_for_route(
        self: &Arc<Self>,
        id: &str,
    ) -> Option<Arc<Controller>> {
        self.session_for_route(id)
            .await
            .map(|session| Arc::clone(&session.ctrl))
    }

    /// The session a route names: the open one, else one this server evicted and
    /// can re-open from disk. `None` when it is neither, so a client that closed
    /// a session still gets the 404 it always did.
    ///
    /// A re-open goes through [`Self::resume_or_create`], which serializes on
    /// `open_gate`, so concurrent requests for one evicted id open it once.
    pub(super) async fn session_for_route(self: &Arc<Self>, id: &str) -> Option<Arc<OpenSession>> {
        if let Some(open) = self.lookup(id) {
            return Some(open);
        }
        if !self.was_evicted(id) {
            return None;
        }
        match self.resume_or_create(id, None).await {
            Ok((session, _created)) => Some(session),
            // The id stays evicted only until the next request; this one gets
            // the same 404 a request for a closed session gets.
            Err(message) if message == SESSION_NOT_FOUND => None,
            Err(message) => {
                self.log.warn(
                    "session_reopen_failed",
                    &[("session_id", id.to_string()), ("error", message)],
                );
                None
            }
        }
    }

    pub(super) fn was_evicted(&self, id: &str) -> bool {
        self.evicted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(&id.to_string())
    }

    /// Remembers that `id` was evicted, so a later request re-opens it. Drops
    /// the oldest id once [`REMEMBERED_EVICTIONS`] are remembered.
    pub(super) fn remember_evicted(&self, id: &str) {
        let mut evicted = self
            .evicted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if evicted.iter().any(|known| known == id) {
            return;
        }
        if evicted.len() == REMEMBERED_EVICTIONS {
            evicted.pop_front();
        }
        evicted.push_back(id.to_string());
    }

    /// Forgets an eviction: the session is open again.
    pub(super) fn forget_evicted(&self, id: &str) {
        let mut evicted = self
            .evicted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(position) = evicted.iter().position(|known| known == id) {
            evicted.remove(position);
        }
    }

    /// Pins `id` open for a live frontend connection, so the sweeper leaves it
    /// alone. `false` when no such session is open, which a caller that has just
    /// opened one never sees.
    pub fn pin_session(&self, id: &str) -> bool {
        let Some(session) = self.lookup(id) else {
            return false;
        };
        // Under the state lock, and only while the registry still holds this
        // session, so a sweep that removes it ([`Self::remove_if_idle`]) either
        // sees the pin or makes this call return `false`.
        let state = session.lock();
        if state.is_closing() {
            return false;
        }
        let still_open = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, &session));
        if !still_open {
            return false;
        }
        session.pins.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Releases one [`Self::pin_session`] hold.
    pub fn unpin_session(&self, id: &str) {
        let Some(session) = self.lookup(id) else {
            return;
        };
        let _ = session
            .pins
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            });
    }

    pub(super) fn lookup(&self, id: &str) -> Option<Arc<OpenSession>> {
        if self.permanently_unavailable(id) {
            return None;
        }
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(id)
            .cloned();
        if let Some(session) = &session {
            session.touch();
        }
        session
    }

    pub(super) fn permanently_unavailable(&self, id: &str) -> bool {
        self.permanent_deletions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(id)
    }

    pub(super) fn remove(&self, id: &str) -> Option<Arc<OpenSession>> {
        let session = self.lookup(id)?;
        let mut state = session.lock();
        if !self.remove_if_same(id, &session) {
            return None;
        }
        state.begin_close();
        drop(state);
        Some(session)
    }

    pub(super) fn all_sessions(&self) -> Vec<Arc<OpenSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub(super) fn register(self: &Arc<Self>, ctrl: Controller) -> Arc<OpenSession> {
        let id = ctrl.info().session_id;
        let session = OpenSession::new(ctrl);
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id.clone(), Arc::clone(&session));
        self.forget_evicted(&id);
        self.metrics.sessions_open(1);
        self.status_changed.send_modify(|version| *version += 1);
        self.restore_queued_turns(&session);
        self.start_wake_loop(&session);
        session
    }

    /// Removes `id` when the registry still holds exactly `session`, so a sweep
    /// that lost a race never evicts a session that was deleted, or re-opened as
    /// a new one, while it was deciding.
    pub(super) fn remove_if_same(&self, id: &str, session: &Arc<OpenSession>) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match sessions.get(id) {
            Some(current) if Arc::ptr_eq(current, session) => {
                sessions.remove(id);
                self.status_changed.send_modify(|version| *version += 1);
                true
            }
            _ => false,
        }
    }

    /// Cancels everything running on `session` and closes its controller. The
    /// session's files, its saved queued turns and its eviction marker stay as
    /// they are, so a later open restores it; [`delete_session`] clears the
    /// marker and the journal itself.
    pub(super) async fn close_session(&self, session: &Arc<OpenSession>) -> Result<(), String> {
        session.cancel_work();
        // Awaited before the blocking close, exactly as `Server::close` does:
        // `Controller::close` only spawns the MCP shutdown.
        session.ctrl.close_resources().await;
        let ctrl = Arc::clone(&session.ctrl);
        match tokio::task::spawn_blocking(move || ctrl.close()).await {
            Ok(result) => result,
            Err(error) => Err(error.to_string()),
        }
    }

    /// Closes every open session that has been idle for
    /// [`Self::session_idle_timeout`] and has nothing running, then the
    /// least-recently-active idle sessions while the registry holds more than
    /// [`Self::max_open_sessions`]. Returns the ids it closed; `None` in either
    /// setting turns that rule off, and both off close nothing.
    ///
    /// A session closed here keeps its files and its saved queued turns, and a
    /// request that names it re-opens it ([`Self::session_for_route`]), so a
    /// client never notices. Busy work -- a running or queued turn, a
    /// compaction, a waiting approval, an unfinished sub-agent
    /// task or an undelivered notification -- keeps a session whatever its age,
    /// so an idle sweeper never truncates real work. Pinned sessions are
    /// skipped: a live frontend connection holds them.
    pub(crate) async fn sweep_idle_sessions(&self) -> Vec<String> {
        let (idle_for, max_open) = (self.session_idle_timeout, self.max_open_sessions);
        if idle_for.is_none() && max_open.is_none() {
            return Vec::new();
        }
        let now = activity_now();
        // With no idle window nothing is ever aged; the cap alone decides.
        let threshold = idle_for.map_or(u64::MAX, |idle_for| idle_for.as_millis() as u64);
        // Snapshot the registry, then judge every candidate outside its lock.
        let mut candidates: Vec<(u64, String, Arc<OpenSession>)> = self
            .all_sessions()
            .into_iter()
            .filter(|session| !session.pinned())
            .map(|session| {
                let idle = session.idle_millis(now);
                (idle, session.ctrl.info().session_id, session)
            })
            .collect();
        // Most idle first, which is also the cap's eviction order.
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));

        let mut remaining = candidates.len();
        let mut victims: Vec<(u64, String, Arc<OpenSession>)> = Vec::new();
        for (idle, id, session) in candidates {
            let aged = idle >= threshold;
            let over_cap = max_open.is_some_and(|max_open| remaining > max_open);
            if !aged && !over_cap {
                break;
            }
            if !self.idle_session(&session, &session.lock()) {
                continue;
            }
            victims.push((idle, id, session));
            remaining -= 1;
        }

        let mut evicted = Vec::with_capacity(victims.len());
        for (idle, id, session) in victims {
            // A delete, a request that re-opened the session, or a turn, pin
            // or other work that started while this sweep decided, already
            // answered for this id.
            if !self.remove_if_idle(&id, &session) {
                continue;
            }
            self.metrics.sessions_open(-1);
            self.metrics.session_evicted();
            self.remember_evicted(&id);
            let fields = [("session_id", id.clone()), ("idle_ms", idle.to_string())];
            self.log.info("session_evicted", &fields);
            if let Err(error) = self.close_session(&session).await {
                self.log.error(
                    "session_close_error",
                    &[("session_id", id.clone()), ("error", error)],
                );
            }
            evicted.push(id);
        }
        evicted
    }

    /// [`Self::remove_if_same`] when `session` is still idle, judged while
    /// holding its state lock. [`Server::start_turn`] and [`Self::pin_session`]
    /// take that lock before changing the session, so no turn or pin starts
    /// between the check and the removal. The lock order is session state, then
    /// the registry; nothing takes them the other way round.
    pub(super) fn remove_if_idle(&self, id: &str, session: &Arc<OpenSession>) -> bool {
        let mut state = session.lock();
        if !self.idle_session(session, &state) || !self.remove_if_same(id, session) {
            return false;
        }
        state.begin_close();
        true
    }

    /// Whether `session`, whose locked state is `state`, has nothing to lose:
    /// no turn, compaction, waiting approval, unfinished sub-agent
    /// task, undelivered notification or pin.
    pub(super) fn idle_session(&self, session: &OpenSession, state: &super::SessionState) -> bool {
        if state.is_closing() || state.busy() || state.waiting.is_some() || session.pinned() {
            return false;
        }
        match session.ctrl.tasks() {
            Some(tasks) => {
                tasks.pending() == 0 && tasks.list().iter().all(|task| task.status.final_status())
            }
            None => true,
        }
    }

    /// Runs [`Self::sweep_idle_sessions`] every [`SESSION_SWEEP_INTERVAL`] with
    /// this server's configured idle window and cap, until `cancel` is
    /// cancelled. `otto serve` starts one per process.
    pub fn start_session_sweeper(self: &Arc<Self>, cancel: CancellationToken) {
        let server = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SESSION_SWEEP_INTERVAL);
            // The first tick of a fresh interval is immediate; a server that
            // just started has nothing to sweep.
            tick.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    _ = tick.tick() => {
                        server.sweep_idle_sessions().await;
                    }
                }
            }
        });
    }

    /// A hit in the registry returns the existing session without calling
    /// `open` again.
    pub(super) async fn resume_or_create(
        self: &Arc<Self>,
        id: &str,
        workspace: Option<&str>,
    ) -> Result<(Arc<OpenSession>, bool), String> {
        if self.permanently_unavailable(id) {
            return Err(SESSION_NOT_FOUND.to_string());
        }
        if id.is_empty() {
            let ctrl = self.factory.create(workspace).await?;
            return Ok((self.register(ctrl), true));
        }
        if let Some(existing) = self.lookup(id) {
            if workspace.is_some_and(|path| existing.ctrl.info().workspace != path) {
                return Err(SESSION_NOT_FOUND.to_string());
            }
            return Ok((existing, false));
        }
        let _gate = self.open_gate.lock().await;
        if self.permanently_unavailable(id) {
            return Err(SESSION_NOT_FOUND.to_string());
        }
        // Re-check: another resume of this id may have finished while we
        // waited for the gate.
        if let Some(existing) = self.lookup(id) {
            if workspace.is_some_and(|path| existing.ctrl.info().workspace != path) {
                return Err(SESSION_NOT_FOUND.to_string());
            }
            return Ok((existing, false));
        }
        let ctrl = self.factory.open(id, workspace).await?;
        Ok((self.register(ctrl), false))
    }

    // ---- wire ----

    pub(super) fn session_context_metrics(&self) -> Vec<SessionContext> {
        self.all_sessions()
            .iter()
            .map(|session| {
                let info = session.ctrl.info();
                SessionContext {
                    session_id: info.session_id,
                    provider: info.provider_type,
                    model: info.model,
                    context_window: info.context_window,
                    context_input_tokens: info.context_input_tokens,
                    context_input_tokens_present: info.context_input_tokens_present,
                    context_input_tokens_pending: info.context_input_tokens_pending,
                }
            })
            .collect()
    }
}
