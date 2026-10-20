//! Applying a run's memory and skill proposals, and recording the run.

use super::*;

impl Reflector {
    /// Runs each skill candidate through the rest of the vetting pipeline
    /// (structure and rule scan, then the model review) and writes the ones
    /// that pass. Each failure is counted on the report by reason.
    pub(super) async fn apply_skills(
        &self,
        context: &Context<'_>,
        run_id: &str,
        shown: &[transcript::Entry],
        candidates: Vec<guard::Candidate>,
        cancel: &CancellationToken,
        report: &mut Report,
    ) -> Result<(), Error> {
        let (Some(store), Some(roots)) = (&self.store, context.skill_roots) else {
            return Ok(());
        };
        let by_id: std::collections::HashMap<&str, &transcript::Entry> = shown
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect();
        let redact = |text: &str| context.runner.redact_text(text);
        let mut remaining = candidates.into_iter();
        while let Some(candidate) = remaining.next() {
            let item = skill_item(&candidate);
            let reject = |report: &mut Report, reason: &'static str| {
                *report.dropped.entry(reason).or_default() += 1;
                let mut item = item.clone();
                item.outcome = runlog::Outcome::Dropped(reason);
                report.items.push(item);
            };
            let checked = match guard::check(candidate, &by_id, &redact) {
                Ok(checked) => checked,
                Err(reason) => {
                    reject(report, reason);
                    continue;
                }
            };
            // Refuse what cannot be written before spending a review call.
            let candidate = checked.candidate();
            if let Err(reason) = skillwrite::precheck(
                store.as_ref(),
                roots,
                candidate.action,
                &candidate.name,
                self.config.max_generated_skills,
            ) {
                reject(report, reason);
                continue;
            }
            if let Err(reason) = skillwrite::validate_python(&candidate.scripts) {
                reject(report, reason);
                continue;
            }
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let verdict = if self.config.skill_review {
                let task_id = format!("reflection:{run_id}:review");
                match review::review(context.runner, checked.candidate(), &task_id, cancel).await {
                    Ok(review::Outcome::Allow) => guard::Verdict::Allowed,
                    Ok(review::Outcome::Reject(_)) => {
                        reject(report, "review_rejected");
                        continue;
                    }
                    Err(review::Failure::Cancelled) => {
                        // Not counted in `dropped`: a canceled run is retried.
                        let mut current = item.clone();
                        current.outcome = runlog::Outcome::Dropped("canceled");
                        report.items.push(current);
                        for candidate in remaining {
                            let mut item = skill_item(&candidate);
                            item.outcome = runlog::Outcome::Dropped("canceled");
                            report.items.push(item);
                        }
                        return Err(Error::Cancelled);
                    }
                    Err(review::Failure::Unavailable(_)) => {
                        reject(report, "review_failed");
                        continue;
                    }
                }
            } else {
                guard::Verdict::NotRequested
            };
            let vetted = guard::approve(checked, verdict);
            if let Some(approvals) = context.approvals {
                match crate::approval::reflection::publish(
                    approvals,
                    store.clone(),
                    roots,
                    self.config.max_generated_skills,
                    &context.runner.session().header().workspace,
                    context.session_id,
                    context.session_path,
                    context.presentation_origin,
                    run_id,
                    &vetted,
                    self.config.skill_review,
                ) {
                    Ok(request) => {
                        let mut item = item.clone();
                        item.outcome = runlog::Outcome::Approval(request.id.clone());
                        report.items.push(item);
                        report.skill_approvals.push(request.id);
                    }
                    Err(_) => reject(report, "skill_approval_failed"),
                }
                continue;
            }
            reject(report, "skill_approval_unavailable");
        }
        Ok(())
    }

    /// Queues each proposal as a candidate, dropping what the store refuses
    /// and what is already pending for review. The runner's advisor, when it
    /// has one, asks for review hints on all proposals concurrently first; a
    /// canceled run queues nothing further.
    pub(super) async fn apply(
        &self,
        context: &Context<'_>,
        run_id: &str,
        plan: Plan,
        report: &mut Report,
        cancel: &CancellationToken,
    ) {
        let seen = known_candidates(context);
        let mut requests = Vec::new();
        // Indexes into `report.items` of the proposals sent to the store, in
        // request order.
        let mut sent = Vec::new();
        for proposal in plan.proposals {
            let scope = match proposal.scope {
                ScopeChoice::User => context.user_scope.clone(),
                ScopeChoice::Workspace => context.workspace_scope.clone(),
            };
            let mut item = memory_item(&proposal);
            if seen.iter().any(|known| known.matches(&scope, &proposal)) {
                *report.dropped.entry("duplicate_candidate").or_default() += 1;
                item.outcome = runlog::Outcome::Dropped("duplicate_candidate");
                report.items.push(item);
                continue;
            }
            requests.push(propose_request(context, run_id, scope, &proposal));
            sent.push(report.items.len());
            report.items.push(item);
        }
        let results = match &context.runner.advisor {
            Some(advisor) => {
                match advisor
                    .propose(context.service, context.user_scope, requests, cancel)
                    .await
                {
                    Some(results) => results,
                    None => {
                        for index in sent {
                            report.items[index].outcome = runlog::Outcome::Dropped("canceled");
                        }
                        return;
                    }
                }
            }
            None => requests
                .iter()
                .map(|request| context.service.propose(request))
                .collect(),
        };
        for (index, result) in sent.into_iter().zip(results) {
            match result {
                Ok(candidates) => {
                    let ids: Vec<String> = candidates
                        .into_iter()
                        .map(|candidate| candidate.id)
                        .collect();
                    report.items[index].outcome = runlog::Outcome::Queued(ids.join(", "));
                    report.candidates.extend(ids);
                }
                Err(_) => {
                    *report.dropped.entry("store_rejected").or_default() += 1;
                    report.items[index].outcome = runlog::Outcome::Dropped("store_rejected");
                }
            }
        }
    }

    /// Records the run row, then writes the run log when a log directory is
    /// set. A log that cannot be written is reported on the report and does
    /// not fail the run. `error` is the failure text and `answer` the raw
    /// model answer of an invalid answer; both are empty otherwise.
    pub(super) fn finish(
        &self,
        context: &Context<'_>,
        mut row: RunRow,
        mut report: Report,
        error: &str,
        answer: &str,
    ) -> Result<Report, Error> {
        row.finished_at = now();
        if let Some(store) = &self.store {
            store
                .record(&row)
                .map_err(|error| Error::Store(error.to_string()))?;
        }
        if let Some(dir) = &self.log_dir {
            let redact = |text: &str| context.runner.redact_text(text);
            let content = runlog::render(
                &runlog::Input {
                    row: &row,
                    session_path: context.session_path,
                    error,
                    answer,
                    entries: report.entries,
                    truncated: row.truncated,
                    external: &report.external,
                    skills_note: report.skills_note,
                    items: &report.items,
                    retrospective: &report.retrospective,
                },
                &redact,
            );
            report.log = Some(runlog::write(dir, &row.started_at, &row.id, &content));
        }
        Ok(report)
    }
}
