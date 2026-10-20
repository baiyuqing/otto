//! Typed persistence operations for workflow runs and attempt history.
use super::contracts::*;
use super::error::Result;
pub trait WorkflowStore: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn create_run(
        &self,
        id: &str,
        definition: &Definition,
        workspace: &str,
        profile: &str,
        provider: &str,
        model: &str,
        input: &str,
    ) -> Result<Run>;
    /// Atomic operation receipt, run creation and conversational subscription.
    #[allow(clippy::too_many_arguments)]
    fn create_origin_run(
        &self,
        id: &str,
        definition: &Definition,
        workspace: &str,
        runtime: &RuntimeIdentity,
        input: &str,
        session: &str,
        operation: &str,
    ) -> Result<(Run, bool)>;
    /// Reconcile an existing receipt before consulting today's mutable catalog.
    fn origin_run(&self, workspace: &str, session: &str, operation: &str) -> Result<Option<Run>>;
    /// Durable origin runs for this exact source session, never a replacement session.
    fn origin_runs(&self, workspace: &str, session: &str) -> Result<Vec<String>>;
    /// Ordered durable pending events, capped at 64 rows, for an exact destination.
    fn pending_events(
        &self,
        workspace: &str,
        session: &str,
        limit: usize,
    ) -> Result<Vec<EventRecord>>;
    /// Advance only after durable inbox enqueue or transcript input evidence.
    /// A sequence belonging to another source must never be acknowledged.
    fn acknowledge_event(&self, workspace: &str, session: &str, seq: i64) -> Result<()>;
    fn get_run(&self, id: &str) -> Result<Option<Run>>;
    fn fork_run(&self, id: &str, source_run_id: &str, after_step: &str) -> Result<Run>;
    fn claim_ready(&self, run_id: &str, step_id: &str, transcript_path: &str) -> Result<u32>;
    fn finish_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        result: std::result::Result<&str, &str>,
    ) -> Result<()>;
    fn interrupt_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        error: &str,
    ) -> Result<()>;
    /// Interrupts every in-flight attempt of this workspace's runs, records the
    /// cause and settles each run to its durable outcome.
    fn recover(&self, workspace: &str) -> Result<usize>;
    /// Interrupts the run's in-flight attempts, recording `error` on them, and
    /// leaves the run paused for an explicit resume.
    fn pause_run(&self, run_id: &str, error: &str) -> Result<()>;
    fn request_approval(&self, id: &str, run_id: &str, step_id: &str) -> Result<ApprovalRequest>;
    fn respond(&self, request_id: &str, approved: bool) -> Result<String>;
    /// Re-derives the run status from its steps. Idempotent, so a driver may
    /// settle after every batch without duplicating events.
    fn settle(&self, run_id: &str) -> Result<()>;
    /// Re-admits one failed or interrupted step, reviving the steps a failure
    /// canceled, and leaves the run running.
    fn retry(&self, run_id: &str, step_id: &str) -> Result<()>;
    fn cancel_attempt(&self, run_id: &str, step_id: &str, attempt: u32) -> Result<()>;
    fn cancel_run(&self, run_id: &str) -> Result<()>;
    /// One page of runs for the workspace, newest first.
    fn list_runs(&self, workspace: &str, query: &ListQuery) -> Result<Vec<Run>>;
    fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>>;
    fn request(&self, request_id: &str) -> Result<Option<ApprovalRequest>>;
    fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>>;
    fn run_age_ms(&self, run_id: &str) -> Option<u128>;
}
