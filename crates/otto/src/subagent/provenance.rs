//! Trusted in-process execution provenance, never decoded from model input.
//! External authenticated HTTP callers remain a separate authority boundary;
//! this scope does not claim to identify arbitrary subprocess/network clients.
tokio::task_local! { static NON_PARENT: (); }

/// Runs `future` in a scope that marks its tool calls as made by a child or a
/// workflow step, not by the source parent.
pub(crate) async fn non_parent<F: std::future::Future>(future: F) -> F::Output {
    NON_PARENT.scope((), future).await
}

/// Whether the current task runs inside [`non_parent`].
pub(crate) fn is_non_parent() -> bool {
    NON_PARENT.try_with(|_| ()).is_ok()
}
