//! Workflow start admission, based on the execution provenance that
//! `subagent::provenance` records.

// Only `tool::workflow` tests name `workflow::provenance::non_parent`; remove
// this re-export when they import `subagent::provenance` directly.
#[cfg(test)]
pub(crate) use crate::subagent::provenance::non_parent;

pub(crate) fn admit_start() -> Result<(), super::Error> {
    if crate::subagent::provenance::is_non_parent() {
        return Err(super::Error::invalid(
            "children and workflow steps cannot start workflows; return the proposed process to the source parent for authorization",
        ));
    }
    Ok(())
}
