//! Typed persistence operations for workflow runs and attempt history.
use super::contracts::*;
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
    ) -> Result<Run, String>;
    fn get_run(&self, id: &str) -> Result<Option<Run>, String>;
    fn fork_run(&self, id: &str, source_run_id: &str, after_step: &str) -> Result<Run, String>;
    fn claim_ready(
        &self,
        run_id: &str,
        step_id: &str,
        transcript_path: &str,
    ) -> Result<u32, String>;
    fn finish_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        result: Result<&str, &str>,
    ) -> Result<(), String>;
    fn interrupt_attempt(
        &self,
        run_id: &str,
        step_id: &str,
        attempt: u32,
        error: &str,
    ) -> Result<(), String>;
    fn recover(&self, workspace: &str) -> Result<usize, String>;
    fn pause_run(&self, run_id: &str) -> Result<(), String>;
    fn finalize_interrupted_batch(&self, run_id: &str) -> Result<(), String>;
    fn finalize_failed(&self, run_id: &str) -> Result<(), String>;
    fn request_approval(
        &self,
        id: &str,
        run_id: &str,
        prompt: &str,
    ) -> Result<ApprovalRequest, String>;
    fn respond(&self, request_id: &str, approved: bool) -> Result<String, String>;
    fn retry(&self, run_id: &str, step_id: &str) -> Result<(), String>;
    fn cancel_attempt(&self, run_id: &str, step_id: &str, attempt: u32) -> Result<(), String>;
    fn cancel_run(&self, run_id: &str) -> Result<(), String>;
    fn list_runs(&self, workspace: &str) -> Result<Vec<Run>, String>;
    fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, String>;
    fn request(&self, request_id: &str) -> Result<Option<ApprovalRequest>, String>;
    fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, String>;
    fn run_age_ms(&self, run_id: &str) -> Option<u128>;
}
