//! The records a stopped turn writes: `turn_stalled` and
//! `turn_budget_exceeded`. Both hold identifiers, names and counts only.

use otto_core::diagnostics::TurnStop;
use serde_json::{Value, json};

use super::{Scope, safe};

impl Scope {
    pub(super) fn record_stall(&self, stall: &otto_core::agent::progress::Stall) {
        let fields = json!({"kind":stall.kind.as_str(),
            "tool":safe(&stall.tool, &self.redactor), "count":stall.count});
        self.turn_stop("turn_stalled", fields, TurnStop::NoProgress(stall.kind));
    }

    pub(super) fn record_budget_exceeded(&self, exceeded: &otto_core::agent::budget::Exceeded) {
        let fields = json!({"limit":exceeded.kind.as_str(),
            "limit_value":exceeded.limit.get(), "used":exceeded.used});
        self.turn_stop(
            "turn_budget_exceeded",
            fields,
            TurnStop::Budget(exceeded.kind),
        );
    }

    /// Writes one record with the session (and task) ids and calls the
    /// process-wide listener once.
    fn turn_stop(&self, event: &str, fields: Value, stop: TurnStop) {
        let mut record = json!({"schema_version":1, "event":event, "session_id":self.session});
        if !self.task.is_empty() {
            record["task_id"] = json!(self.task);
        }
        if let (Some(record), Some(fields)) = (record.as_object_mut(), fields.as_object()) {
            record.extend(fields.clone());
        }
        self.log.write(record);
        if let Some(listener) = self.stops.get() {
            listener(stop);
        }
    }
}
