//! Custom entries held back until the session file is created.

use super::*;

/// One custom entry queued by [`Store::append_custom_entry_when_created`].
#[derive(Debug)]
pub(crate) struct DeferredCustomEntry {
    pub(crate) custom_type: String,
    pub(crate) data: Box<serde_json::value::RawValue>,
}

impl Store {
    /// Appends a `custom` entry like [`Store::append_custom_entry`] when the
    /// session file exists. While the file does not exist, nothing is written
    /// and the entry is queued instead: the file creation that the next
    /// durable write triggers writes the queued entries directly after the
    /// header and the `otto.runtime` entry, in call order, before the entry
    /// that triggered creation. A store closed before then drops the queue.
    ///
    /// Only plain custom types are accepted: `otto.operation` and `otto.plan`
    /// carry in-memory side effects and return an invalid-argument error.
    /// `data` must be valid JSON; a malformed value fails at this call.
    pub fn append_custom_entry_when_created(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), PiError> {
        if custom_type == OPERATION_CUSTOM_TYPE
            || custom_type == otto_core::session::PLAN_CUSTOM_TYPE
        {
            return Err(PiError::invalid(format!(
                "custom type {custom_type} cannot be deferred until the session file is created"
            )));
        }
        let mut state = self.lock()?;
        state.writable()?;
        if state.file.is_some() {
            drop(state);
            return self.append_custom_entry(custom_type, data);
        }
        // Validate first so malformed custom JSON still reports an error.
        raw_value(data.to_owned())?;
        let data = raw_value(recording::custom_data(custom_type, data))?;
        state.deferred_custom_entries.push(DeferredCustomEntry {
            custom_type: custom_type.to_owned(),
            data,
        });
        Ok(())
    }
}
