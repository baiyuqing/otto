//! Human command receipts: independent of the agent transcript and observations.
use super::Controller;
use super::management::{ManagementError, ManagementErrorKind, Reply, Request};
use otto_core::agent::redactor::Redactor;
use otto_core::session::local_command::{LocalCommandReceipt, LocalCommandStatus};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(1);

impl Controller {
    /// Execute a memory slash command, including usage errors, without a model turn.
    pub fn memory_command(&self, name: &str, args: &str) -> Result<Reply, ManagementError> {
        if !matches!(name, "memory" | "remember") {
            return Err(ManagementError::new(
                ManagementErrorKind::Invalid,
                "not a memory command",
            ));
        }
        match Request::parse(name, args) {
            Ok(request) => self.management_typed(request),
            Err(usage) => {
                let mut receipt = self.begin_local_command(&format!("/{name}"), args)?;
                receipt.status = LocalCommandStatus::Error;
                receipt.summary = "invalid command".into();
                receipt.output = usage.clone();
                self.record_local_command(&receipt)?;
                Err(ManagementError::new(ManagementErrorKind::Invalid, usage))
            }
        }
    }

    pub(super) fn receipt_management(&self, request: Request) -> Result<Reply, ManagementError> {
        let (command, parameters) = request_metadata(&request);
        let mut receipt = self.begin_local_command(command, &parameters)?;
        let result = self.management_unrecorded(request);
        match &result {
            Ok(reply) => {
                receipt.status = LocalCommandStatus::Success;
                receipt.summary = reply_summary(reply);
                receipt.output = match reply {
                    Reply::Remembered(_)
                    | Reply::MemoryForgotten { .. }
                    | Reply::MemoryStateChanged(_)
                    | Reply::MemoryRestored(_)
                    | Reply::MemoryVerified(_)
                    | Reply::MemoryReviewed(_, _) => String::new(),
                    _ => memory_output(reply),
                };
                if let Reply::MemoryServiceResult { status, .. } = reply {
                    receipt.status = status.clone();
                }
                if matches!(
                    reply,
                    Reply::MemoryProposed(
                        crate::memory::proposals::Outcome::Unavailable(_)
                            | crate::memory::proposals::Outcome::Rejected(_)
                    )
                ) {
                    receipt.status = LocalCommandStatus::Error;
                }
                if matches!(reply, Reply::Usage(_)) {
                    receipt.status = LocalCommandStatus::Error;
                }
            }
            Err(error) => {
                receipt.status = LocalCommandStatus::Error;
                receipt.summary = "command failed".into();
                receipt.output = error.message.clone();
            }
        }
        self.record_local_command(&receipt)?;
        result
    }

    fn begin_local_command(
        &self,
        command: &str,
        parameters: &str,
    ) -> Result<LocalCommandReceipt, ManagementError> {
        let now = chrono::Utc::now();
        let receipt = LocalCommandReceipt {
            id: format!(
                "{}-{}",
                now.timestamp_micros(),
                NEXT_INVOCATION.fetch_add(1, Ordering::Relaxed)
            ),
            command: command.into(),
            parameters: parameters.into(),
            status: LocalCommandStatus::Pending,
            summary: "outcome pending; reconcile before retrying".into(),
            output: String::new(),
            created_at: now.to_rfc3339(),
            after_messages: self.history().len(),
        };
        self.record_local_command(&receipt)?;
        Ok(receipt)
    }

    fn record_local_command(&self, receipt: &LocalCommandReceipt) -> Result<(), ManagementError> {
        let fail = |message| ManagementError::new(ManagementErrorKind::Internal, message);
        if !self.dynamic_content_available() {
            return Err(fail("local command recording unavailable".into()));
        }
        let session = self
            .current_session_opt()
            .ok_or_else(|| fail("session is closed".into()))?;
        let encoded = serde_json::to_string(receipt).map_err(|error| fail(error.to_string()))?;
        let safe = Redactor::new(&self.builder.secret_values(None))
            .redact_record_json(&encoded)
            .get()
            .to_owned();
        session
            .handle()
            .append_local_command(&safe)
            .map_err(|error| {
                fail(format!(
                    "receipt persistence failed; reconcile before retrying: {error}"
                ))
            })
    }
}

pub(super) fn is_memory_request(request: &Request) -> bool {
    matches!(
        request,
        Request::Remember { .. }
            | Request::MemoryList { .. }
            | Request::MemoryShow { .. }
            | Request::MemorySearch { .. }
            | Request::MemoryForget { .. }
            | Request::MemoryHistory { .. }
            | Request::MemoryDiff { .. }
            | Request::MemoryRestore { .. }
            | Request::MemoryBind { .. }
            | Request::MemoryState { .. }
            | Request::MemoryVerify { .. }
            | Request::MemoryPropose { .. }
            | Request::MemoryReviewList
            | Request::MemoryCandidatePage { .. }
            | Request::MemoryReview { .. }
    )
}

fn request_metadata(request: &Request) -> (&'static str, String) {
    use Request::*;
    let command = match request {
        Remember { .. } => "/remember",
        MemoryList { .. } => "/memory list",
        MemoryShow { .. } => "/memory show",
        MemorySearch { .. } => "/memory search",
        MemoryForget { .. } => "/memory forget",
        MemoryHistory { .. } => "/memory history",
        MemoryDiff { .. } => "/memory diff",
        MemoryRestore { .. } => "/memory restore",
        MemoryBind { .. } => "/memory bind",
        MemoryState { .. } => "/memory state",
        MemoryVerify { .. } => "/memory verify",
        MemoryPropose { .. } => "/memory propose",
        _ => "/memory review",
    };
    // Explicitly omit mutation bodies: a receipt must not preserve a second
    // copy of a remembered or retracted record's text.
    let parameters = match request {
        Remember {
            scope, kind, key, ..
        } => format!("scope={scope:?} kind={kind} key={key}"),
        MemoryBind {
            id,
            expected_revision,
            ..
        } => format!("id={id}; expected_revision={expected_revision}"),
        MemoryShow { id }
        | MemoryForget { id, .. }
        | MemoryHistory { id }
        | MemoryRestore { id, .. }
        | MemoryState { id, .. }
        | MemoryVerify { id, .. }
        | MemoryPropose { id, .. }
        | MemoryReview { id, .. }
        | MemoryDiff { id, .. } => format!("id={id}; {request:?}"),
        MemorySearch { query } => format!("query={query}"),
        _ => format!("{request:?}"),
    };
    (command, parameters)
}

fn reply_summary(reply: &Reply) -> String {
    use Reply::*;
    match reply {
        Remembered(record)
        | MemoryRestored(record)
        | MemoryStateChanged(record)
        | MemoryVerified(record) => format!(
            "{} scope={}:{} revision={} state={}",
            record.id,
            record.scope.namespace,
            record.scope.id,
            record.revision,
            record.state.as_str()
        ),
        MemoryReviewed(decision, result) => {
            let mut summary = format!(
                "{} candidate {}",
                if *decision == crate::memory::ReviewDecision::Accept {
                    "accepted"
                } else {
                    "rejected"
                },
                result.candidate.id
            );
            if let Some(record) = &result.record {
                summary.push_str(&format!(
                    " record={} scope={}:{} revision={} state={}",
                    record.id,
                    record.scope.namespace,
                    record.scope.id,
                    record.revision,
                    record.state.as_str()
                ));
            }
            if let Some(tombstone) = &result.tombstone {
                summary.push_str(&format!(" forgot {}", tombstone.id));
            }
            summary
        }
        MemoryForgotten {
            tombstone,
            previous_revision,
            ..
        } => format!("forgot {} revision={previous_revision}", tombstone.id),
        MemoryList(page) => format!("{} records", page.records.len()),
        MemorySearch(result) => format!(
            "{} records, {} candidates",
            result.records.len(),
            result.candidates.len()
        ),
        MemoryProposed(crate::memory::proposals::Outcome::Proposed(candidate)) => format!(
            "proposed candidate={} target={} action={}",
            candidate.id,
            candidate.target_id,
            candidate.action.as_str()
        ),
        MemoryProposed(crate::memory::proposals::Outcome::NoChange { .. }) => {
            "no change proposed".into()
        }
        MemoryProposed(crate::memory::proposals::Outcome::Unavailable(_)) => {
            "proposal unavailable".into()
        }
        MemoryProposed(crate::memory::proposals::Outcome::Rejected(_)) => {
            "proposal rejected".into()
        }
        MemoryCandidates(rows) => format!("{} candidates", rows.len()),
        MemoryCandidatePage(page) => format!("{} candidates", page.candidates.len()),
        MemoryRecord(record) => format!(
            "{} scope={}:{} revision={}",
            record.id, record.scope.namespace, record.scope.id, record.revision
        ),
        MemoryServiceResult { text, .. } | Notice(text) => text.clone(),
        Usage(_) => "invalid command".into(),
        _ => "completed; expand for output".into(),
    }
}

/// Complete safe human-facing query output. Mutations use metadata-only summaries.
fn memory_output(reply: &Reply) -> String {
    format!("{reply:#?}")
}
