//! The loop that drives a run through its steps.

use super::*;

impl Controller {
    pub(super) async fn drive(
        &self,
        run_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let run = self.get(run_id)?;
            if run.status != RunStatus::Running {
                return Ok(());
            }
            let ready: Vec<StepRecord> = run
                .steps
                .iter()
                .filter(|step| step.status == StepStatus::Ready)
                .cloned()
                .collect();
            if ready.is_empty() {
                return Ok(());
            }

            let mut agents = Vec::new();
            for step in ready {
                match step.kind {
                    StepKind::Approval => {
                        let request_id = random_id()
                            .map_err(|_| Error::internal("generate workflow id failed"))?;
                        let request = self.store.request_approval(&request_id, run_id, &step.id)?;
                        if let Some(service) = self
                            .approval_service
                            .lock()
                            .map_err(|_| runtime_unavailable())?
                            .as_ref()
                            .and_then(std::sync::Weak::upgrade)
                        {
                            crate::workflow::approval::publish(&service, &self.workspace, &request)
                                .map_err(approval_failure)?;
                        }
                    }
                    StepKind::Agent | StepKind::Handoff => agents.push(step),
                }
            }
            if agents.is_empty() {
                continue;
            }

            let mut attempts = JoinSet::new();
            for step in agents {
                let permit = Arc::clone(&self.semaphore)
                    .acquire_owned()
                    .await
                    .map_err(|_| runtime_unavailable())?;
                if cancel.is_cancelled() {
                    drop(permit);
                    return Ok(());
                }
                let next_attempt = step.attempt.saturating_add(1);
                let run_root = self.transcript_root.join(run_id);
                let step_root = run_root.join(&step.id);
                let attempt_root = step_root.join(next_attempt.to_string());
                for directory in [&self.transcript_root, &run_root, &step_root, &attempt_root] {
                    secure_directory(directory)?;
                }
                let session_id = random_id()
                    .map_err(|_| Error::internal("generate workflow session id failed"))?;
                let workspace_key = otto_session::workspace_key(Path::new(&run.workspace))
                    .map_err(|_| Error::unavailable("resolve workflow transcript path failed"))?;
                let transcript_path = attempt_root
                    .join(workspace_key)
                    .join(format!("{session_id}.jsonl"))
                    .to_string_lossy()
                    .into_owned();
                let attempt_number = self.store.claim_ready(run_id, &step.id, &transcript_path)?;
                let attempt = Attempt {
                    run_id: run_id.to_string(),
                    step_id: step.id.clone(),
                    agent: step.agent.clone(),
                    agent_definition: run
                        .definition
                        .agents
                        .iter()
                        .find(|definition| definition.name == step.agent)
                        .cloned()
                        .ok_or_else(|| Error::internal("workflow agent snapshot is missing"))?,
                    prompt: attempt_prompt(&run, &step)?,
                    attempt: attempt_number,
                    session_id,
                    transcript_root: attempt_root.to_string_lossy().into_owned(),
                    transcript_path,
                };
                let executor = Arc::clone(&self.executor);
                let parent_cancel = cancel.clone();
                let started = Instant::now();
                let deadlines = self.deadlines;
                attempts.spawn(async move {
                    let deadline = deadlines
                        .step_timeout
                        .map_or_else(Deadline::unlimited, Deadline::after);
                    let control = Control::new(deadline);
                    let execute = crate::subagent::provenance::non_parent(executor.execute(attempt.clone(), &control));
                    tokio::pin!(execute);
                    let mut timed_out = false;
                    let result = tokio::select! {
                        biased;
                        result = &mut execute => result,
                        () = parent_cancel.cancelled() => {
                            control.stop(otto_core::model::OperationStopReason::UserCancellation);
                            execute.await
                        }
                        () = deadline.expired() => {
                            timed_out = true;
                            control.stop(otto_core::model::OperationStopReason::Deadline);
                            // Grace is only an observation window. Keep owning
                            // the effectful future until it safely settles.
                            match tokio::time::timeout(deadlines.cancellation_grace, &mut execute).await {
                                Ok(result) => result,
                                Err(_) => execute.await,
                            }
                        }
                    };
                    drop(permit);
                    (
                        attempt,
                        result,
                        parent_cancel.is_cancelled(),
                        timed_out,
                        started,
                    )
                });
            }

            let mut failed = false;
            let mut timed_out = false;
            while let Some(joined) = attempts.join_next().await {
                let (attempt, result, canceled, attempt_timed_out, started) =
                    joined.map_err(|_| Error::internal("workflow attempt task failed"))?;
                if attempt_timed_out {
                    self.store.interrupt_attempt(
                        &attempt.run_id,
                        &attempt.step_id,
                        attempt.attempt,
                        "workflow step deadline exceeded",
                    )?;
                    self.log_step_finished(&run, &attempt.step_id, "interrupted", started);
                    timed_out = true;
                    continue;
                }
                if canceled {
                    self.store.cancel_attempt(
                        &attempt.run_id,
                        &attempt.step_id,
                        attempt.attempt,
                    )?;
                    self.log_step_finished(&run, &attempt.step_id, "canceled", started);
                    continue;
                }
                match result {
                    Ok(result) => {
                        self.store.finish_attempt(
                            &attempt.run_id,
                            &attempt.step_id,
                            attempt.attempt,
                            Ok(&result),
                        )?;
                        self.log_step_finished(&run, &attempt.step_id, "succeeded", started);
                    }
                    Err(error) => {
                        self.store.finish_attempt(
                            &attempt.run_id,
                            &attempt.step_id,
                            attempt.attempt,
                            Err(&error),
                        )?;
                        self.log_step_finished(&run, &attempt.step_id, "failed", started);
                        cancel.cancel();
                        failed = true;
                    }
                }
            }
            // A batch canceled by `cancel` or `close` belongs to that caller: the
            // driver leaves the run status to it. A batch whose own failure
            // canceled its attempts must still settle below.
            if cancel.is_cancelled() && !failed && !timed_out {
                return Ok(());
            }
            // Each settled attempt settles the run in its own transaction; settle
            // once more so a batch that ended in failure or timeout can never leave
            // the run `running` with no owner.
            self.store.settle(run_id)?;
            if timed_out || failed {
                self.log_run_if_final(&self.get(run_id)?);
                return Ok(());
            }
            self.log_run_if_final(&self.get(run_id)?);
        }
    }
}
