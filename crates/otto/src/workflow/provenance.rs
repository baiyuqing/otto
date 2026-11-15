//! Workflow start admission, based on the execution provenance that
//! `subagent::provenance` records.

pub(crate) fn admit_start() -> Result<(), super::Error> {
    if crate::subagent::provenance::is_non_parent() {
        return Err(super::Error::invalid(
            "children and workflow steps cannot start workflows; return the proposed process to the source parent for authorization",
        ));
    }
    Ok(())
}
