//! Trusted in-process execution provenance, never decoded from model input.
//! External authenticated HTTP callers remain a separate authority boundary;
//! this scope does not claim to identify arbitrary subprocess/network clients.
tokio::task_local! { static NON_PARENT: (); }

pub(crate) async fn non_parent<F: std::future::Future>(future: F) -> F::Output {
    NON_PARENT.scope((), future).await
}
pub(crate) fn admit_start() -> Result<(), super::Error> {
    if NON_PARENT.try_with(|_| ()).is_ok() {
        return Err(super::Error::invalid(
            "children and workflow steps cannot start workflows; return the proposed process to the source parent for authorization",
        ));
    }
    Ok(())
}
