//! Marks a run that blocks on a user decision, so the wait does not count
//! against `[agent.budget].max_duration`.
//!
//! The tool registry runs every tool call of an agent run inside [`scoped`]
//! with the run's holder (see [`otto_core::agent::budget::Holder`]).
//! [`Service::wait`](super::Service::wait) calls [`pause`] while the request it
//! waits for needs a human decision. A wait outside a scope (a frontend
//! waiting for a decision, a test) pauses nothing.

use std::future::Future;

use otto_core::agent::budget::{HolderHandle, UserPause};

tokio::task_local! {
    static HOLDER: HolderHandle;
}

/// Runs `future` so that a [`pause`] made while it is polled belongs to
/// `holder`. Without a holder the future runs unchanged.
pub(crate) async fn scoped<F: Future>(holder: Option<HolderHandle>, future: F) -> F::Output {
    match holder {
        Some(holder) => HOLDER.scope(holder, future).await,
        None => future.await,
    }
}

/// Marks the holder of the surrounding [`scoped`] run as waiting for a user
/// decision until the returned guard is dropped; `None` outside a scope.
pub(crate) fn pause() -> Option<UserPause> {
    HOLDER.try_with(HolderHandle::pause_for_user).ok()
}
