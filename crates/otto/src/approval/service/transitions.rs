//! Withdraw, supersede, application, mutation and waiting for requests.

use super::*;

impl Service {
    pub fn withdraw(&self, id: &str, scope: &Scope, revision: u64) -> Result<Request> {
        self.mutate(id, scope, |request| {
            if request.revision != revision {
                return Err(Error::Conflict);
            }
            if request.state == State::Withdrawn {
                return Ok(false);
            }
            if request.state != State::Pending {
                return Err(Error::Conflict);
            }
            request.state = State::Withdrawn;
            Ok(true)
        })
    }
    pub(crate) fn supersede_unstarted(&self, expected: &Request) -> Result<Request> {
        self.mutate(&expected.id, &expected.scope, |current| {
            if current == expected
                && matches!(current.state, State::Pending | State::Approved)
                && current.application == ApplicationState::NotStarted
            {
                current.state = State::Superseded;
                if let Some(review) = current.review.as_mut() {
                    review.status = ReviewStatus::Completed;
                }
                return Ok(true);
            }
            if current.state == State::Superseded && current.subject == expected.subject {
                return Ok(false);
            }
            Err(Error::Conflict)
        })
    }
    pub fn application(
        &self,
        id: &str,
        scope: &Scope,
        expected: ApplicationState,
        next: ApplicationState,
        error: &str,
    ) -> Result<Request> {
        self.mutate(id, scope, |request| {
            if !matches!(
                request.state,
                State::Approved | State::Rejected | State::Withdrawn | State::Expired
            ) || request.application != expected
            {
                return Err(Error::Conflict);
            }
            if next == ApplicationState::Applying
                && request.state == State::Approved
                && request
                    .expires_at
                    .is_some_and(|time| time <= Utc::now().timestamp())
            {
                return Err(Error::Conflict);
            }
            if !matches!(
                (expected, next),
                (ApplicationState::NotStarted, ApplicationState::Applying)
                    | (
                        ApplicationState::Applying,
                        ApplicationState::Applied
                            | ApplicationState::Failed
                            | ApplicationState::Unknown
                    )
            ) {
                return Err(Error::Invalid("invalid application transition"));
            }
            request.application = next;
            request.application_error = error.into();
            Ok(true)
        })
    }
    pub(in crate::approval) fn mutate(
        &self,
        id: &str,
        scope: &Scope,
        update: impl Fn(&mut Request) -> Result<bool>,
    ) -> Result<Request> {
        self.mutate_checked(id, scope, false, update)
    }
    pub(super) fn mutate_checked(
        &self,
        id: &str,
        scope: &Scope,
        automatic: bool,
        update: impl Fn(&mut Request) -> Result<bool>,
    ) -> Result<Request> {
        loop {
            let expected = self.store.load(id)?;
            if &expected.scope != scope {
                return Err(Error::Unauthorized);
            }
            let mut request = expected.clone();
            if update(&mut request)? {
                let claim = if request.application != expected.application {
                    if request.application == ApplicationState::Applying {
                        let (pid, started) = crate::process_identity::current_process();
                        ClaimChange::Start(Claim {
                            owner: self.owner.clone(),
                            pid,
                            started,
                            native: expected.state == State::Pending,
                        })
                    } else if expected.application == ApplicationState::Applying {
                        ClaimChange::Finish(self.owner.clone())
                    } else {
                        ClaimChange::None
                    }
                } else {
                    ClaimChange::None
                };
                request = match self.store.commit(&expected, &request, claim, automatic) {
                    Ok(request) => request,
                    Err(Error::Conflict) if self.store.load(id)? != expected => {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            }
            self.changed();
            return Ok(request);
        }
    }
    pub async fn wait(
        &self,
        id: &str,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Request> {
        self.wait_until(id, scope, cancel, false).await
    }
    /// Wait for review to finish or become an actionable human question.
    /// The original executor keeps using `wait` until a terminal decision.
    pub async fn wait_actionable(
        &self,
        id: &str,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Request> {
        self.wait_until(id, scope, cancel, true).await
    }
    pub(super) async fn wait_until(
        &self,
        id: &str,
        scope: &Scope,
        cancel: &CancellationToken,
        actionable: bool,
    ) -> Result<Request> {
        let mut changes = self.changes();
        let mut poll = tokio::time::interval(Duration::from_millis(200));
        loop {
            if cancel.is_cancelled() {
                let request = self.get(id, &scope.workspace, Some(&scope.session_id))?;
                if request.state == State::Pending {
                    return self.withdraw(id, scope, request.revision);
                }
                return Ok(request);
            }
            let mut request = self.auto_approve(id, scope)?;
            if request.state != State::Pending {
                return Ok(request);
            }
            if request.review.as_ref().is_some_and(|review| {
                matches!(
                    review.status,
                    ReviewStatus::Queued | ReviewStatus::Reviewing
                )
            }) {
                self.review_once(&request, cancel).await?;
                request = self.store.load(id)?;
                if request.state != State::Pending || cancel.is_cancelled() {
                    continue;
                }
            }
            if request
                .expires_at
                .is_some_and(|time| time <= Utc::now().timestamp())
            {
                return self.mutate(id, scope, |r| {
                    if r.state == State::Pending {
                        r.state = State::Expired;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                });
            }
            if actionable && request.is_human_pending() {
                return Ok(request);
            }
            tokio::select! {
                () = cancel.cancelled() => {
                    match self.withdraw(id,scope,request.revision) { Ok(r) => return Ok(r), Err(Error::Conflict) => {}, Err(e) => return Err(e) }
                    return self.get(id,&scope.workspace,Some(&scope.session_id));
                },
                _ = changes.changed() => {},
                _ = poll.tick() => {},
            }
        }
    }
}
