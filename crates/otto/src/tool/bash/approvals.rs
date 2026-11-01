//! Approval grants and excluded-command matching for the bash tool.

use super::*;

impl BashApprovals {
    pub fn new(executor: Arc<dyn CommandExecutor>, environment: Vec<String>) -> Self {
        Self {
            executor,
            environment,
            state: Mutex::new(ApprovalState::default()),
            changes: tokio::sync::watch::channel(0).0,
            decision: tokio::sync::Mutex::new(()),
        }
    }

    pub fn request(&self, session_id: &str, command: &str) -> String {
        self.request_read_path(session_id, command, "")
    }

    pub(crate) fn request_read_path(
        &self,
        session_id: &str,
        command: &str,
        read_path: &str,
    ) -> String {
        let mut state = self.state.lock().expect("bash approval mutex");
        if let Some(request) = state
            .requests
            .get(session_id)
            .filter(|request| request.command == command && request.read_path == read_path)
        {
            return request.id.clone();
        }
        state.next_id += 1;
        let id = format!("approval-{}", state.next_id);
        state.requests.insert(
            session_id.to_owned(),
            Approval {
                id: id.clone(),
                command: command.to_owned(),
                read_path: read_path.to_owned(),
                granted: false,
                deciding: false,
            },
        );
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        id
    }

    /// Grants one pending command for `session_id`.
    pub fn approve(&self, session_id: &str, id: &str) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("bash approval mutex");
        let request = state
            .requests
            .get_mut(session_id)
            .filter(|request| request.id == id)
            .ok_or("approval request not found")?;
        request.granted = true;
        Ok(())
    }

    /// The command of `session_id`'s pending request `id`, granted or not.
    pub fn pending_command(&self, session_id: &str, id: &str) -> Option<String> {
        let state = self.state.lock().expect("bash approval mutex");
        state
            .requests
            .get(session_id)
            .filter(|request| request.id == id)
            .map(|request| request.command.clone())
    }

    /// The canonical read path, or an empty string for an elevated command.
    pub fn pending_read_path(&self, session_id: &str, id: &str) -> Option<String> {
        self.state
            .lock()
            .expect("bash approval mutex")
            .requests
            .get(session_id)
            .filter(|request| request.id == id)
            .map(|request| request.read_path.clone())
    }

    /// A snapshot of the ungranted request bound to this session.
    pub fn pending(&self, session_id: &str) -> Option<(String, String, String)> {
        let state = self.state.lock().expect("bash approval mutex");
        state
            .requests
            .get(session_id)
            .filter(|request| !request.granted && !request.deciding)
            .map(|request| {
                (
                    request.id.clone(),
                    request.command.clone(),
                    request.read_path.clone(),
                )
            })
    }

    /// Reserves a user decision before waking the retry loop. It does not
    /// grant execution; the controller still validates and applies any read
    /// grant. Model withdrawal cannot overtake a decision already accepted.
    pub fn reserve(&self, session_id: &str, id: &str) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("bash approval mutex");
        let request = state
            .requests
            .get_mut(session_id)
            .filter(|request| request.id == id && !request.granted && !request.deciding)
            .ok_or("approval is no longer pending")?;
        request.deciding = true;
        Ok(())
    }

    /// Only an ungranted, exact request may be withdrawn. Never undoes execution.
    pub fn revoke(&self, session_id: &str, id: &str) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("bash approval mutex");
        if !state
            .requests
            .get(session_id)
            .is_some_and(|request| request.id == id && !request.granted && !request.deciding)
        {
            return Err("approval is no longer pending");
        }
        state.requests.remove(session_id);
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        Ok(())
    }

    /// Subscribe before testing the request to avoid losing a concurrent withdrawal.
    pub async fn withdrawn(&self, session_id: &str, id: &str) {
        let mut changes = self.changes.subscribe();
        loop {
            if self.pending_command(session_id, id).is_none() {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn discard(&self, session_id: &str, id: &str) {
        let mut state = self.state.lock().expect("bash approval mutex");
        if state
            .requests
            .get(session_id)
            .is_some_and(|request| request.id == id)
        {
            state.requests.remove(session_id);
            self.changes
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    /// Pending approvals for `session_id`: 0 or 1, since a session keeps only
    /// its newest request.
    pub fn pending_count(&self, session_id: &str) -> usize {
        let state = self.state.lock().expect("bash approval mutex");
        usize::from(state.requests.contains_key(session_id))
    }

    pub fn take(&self, session_id: &str, command: &str) -> bool {
        self.take_read_path(session_id, command, "")
    }

    pub(crate) fn take_read_path(&self, session_id: &str, command: &str, read_path: &str) -> bool {
        let mut state = self.state.lock().expect("bash approval mutex");
        let granted = state.requests.get(session_id).is_some_and(|request| {
            request.granted && request.command == command && request.read_path == read_path
        });
        if !granted {
            return false;
        }
        state.requests.remove(session_id);
        true
    }
}
