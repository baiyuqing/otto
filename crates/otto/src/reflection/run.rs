//! `Reflector::run` and the skill-allowance helpers it uses.

use super::*;

impl Reflector {
    /// Runs one reflection over what no earlier run covered.
    pub async fn run(
        &self,
        context: &Context<'_>,
        trigger: Trigger,
        focus: &str,
        cancel: &CancellationToken,
    ) -> Result<Report, Error> {
        if !self.config.enabled {
            return Err(Error::Disabled);
        }
        let enabled = match trigger {
            Trigger::Manual | Trigger::Requested => true,
            Trigger::OnWork => self.auto() == Auto::OnWork,
            Trigger::OnCompaction => {
                matches!(self.auto(), Auto::OnCompaction | Auto::OnWork)
            }
            Trigger::OnExit => self.auto() == Auto::OnExit,
        };
        if !enabled {
            return Err(Error::Disabled);
        }
        if matches!(trigger, Trigger::OnWork | Trigger::OnCompaction)
            && self.too_soon(context.session_id)
        {
            return Ok(Report::skipped("an automatic run started recently"));
        }
        if context.session_path.is_empty() {
            return Err(Error::NoSession);
        }
        if !context.runner.allows_dynamic_content() {
            return Err(Error::BoundaryClosed);
        }
        let run_id = crate::memory::new_id().map_err(|_| Error::Store("run id".into()))?;
        let started_at = now();
        let session_id = context.session_id;
        let watermark = match &self.store {
            Some(store) => store
                .watermark(session_id)
                .map_err(|e| Error::Store(e.to_string()))?,
            None => None,
        };
        let redact = |text: &str| context.runner.redact_text(text);
        let slice = transcript::read_slice(
            Path::new(context.session_path),
            watermark.as_deref(),
            &redact,
        )
        .map_err(Error::Read)?;

        let mut row = RunRow {
            id: run_id.clone(),
            session_id: session_id.to_owned(),
            trigger: trigger.as_str().to_owned(),
            started_at,
            finished_at: String::new(),
            status: Status::Noop,
            from_entry: slice.after_id.clone(),
            to_entry: slice.last_id.clone(),
            input_bytes: 0,
            truncated: false,
            memories_proposed: 0,
            memories_dropped: 0,
            skills_written: 0,
            detail: String::new(),
        };
        let mut report = Report {
            run_id: run_id.clone(),
            status: Status::Noop,
            candidates: Vec::new(),
            dropped: BTreeMap::new(),
            skills: Vec::new(),
            skill_approvals: Vec::new(),
            skills_withheld: false,
            entries: slice.entries.len(),
            tainted: slice.tainted(),
            truncated: false,
            note: String::new(),
            items: Vec::new(),
            external: slice
                .entries
                .iter()
                .filter(|entry| entry.external)
                .map(|entry| {
                    let source = if entry.tool.is_empty() {
                        entry.role.as_str()
                    } else {
                        entry.tool.as_str()
                    };
                    (entry.id.clone(), source.to_owned())
                })
                .collect(),
            skills_note: "",
            log: None,
            retrospective: String::new(),
        };

        let user_turns = slice
            .entries
            .iter()
            .filter(|entry| entry.role == EntryRole::User && !entry.external)
            .count();
        let has_user = user_turns > 0;
        if trigger == Trigger::OnExit && user_turns < self.config.min_turns {
            // Too short to be worth a call. No row and no watermark: the
            // session is ending, and `/reflect` can still cover it later.
            return Ok(Report::skipped("the session is shorter than min_turns"));
        }
        let skills_allowed = self.skills_allowed(context, &slice, &mut report);
        if !has_user || (!self.config.memories && !skills_allowed) {
            return self.finish(context, row, report, "", "");
        }

        let existing = existing_memories(context)?;
        let (shown, truncated) = prompt::fit(&slice.entries, self.config.max_input_bytes);
        let skill_context = skills_allowed.then(|| self.skill_context(context));
        let message = prompt::user_message(
            shown,
            &existing,
            skill_context.as_deref(),
            &bounded_focus(focus),
        );
        row.input_bytes = message.len();
        row.truncated = truncated;
        report.truncated = truncated;

        if !matches!(trigger, Trigger::Manual | Trigger::Requested)
            && let Some(gate) = &self.gate
        {
            // Reuse only the bounded, redacted evidence. No standing memory,
            // skill contents or model-controlled focus is needed by this hint.
            let state = prompt::user_message(shown, &[], None, "");
            match gate.decide(&state, trigger.as_str(), cancel).await {
                gate::Outcome::Defer => {
                    return Ok(Report::skipped(
                        "automatic reflection deferred; evidence retained",
                    ));
                }
                gate::Outcome::Cancelled => {
                    row.status = Status::Canceled;
                    let _ = self.finish(context, row, report, "", "");
                    return Err(Error::Cancelled);
                }
                gate::Outcome::Immediate => {}
            }
        }

        let request = TextRequest {
            system_prompt: &prompt::system_prompt(self.config.memories, skills_allowed),
            user_text: &message,
            maximum_bytes: MAXIMUM_ANSWER_BYTES,
        };
        let task_id = format!("reflection:{run_id}");
        let answer = match context
            .runner
            .complete_text(&request, &task_id, cancel)
            .await
        {
            Ok(response) => response.text,
            Err(error) if error.is_cancelled() => {
                row.status = Status::Canceled;
                let _ = self.finish(context, row, report, "", "");
                return Err(Error::Cancelled);
            }
            Err(error) => {
                row.status = Status::Failed;
                row.detail = "model_call".into();
                let message = error.to_string();
                let _ = self.finish(context, row, report, &message, "");
                return Err(Error::Model(message));
            }
        };

        let plan = match output::plan(
            &answer,
            shown,
            &existing,
            if self.config.memories {
                self.config.max_memories
            } else {
                0
            },
            skills_allowed.then_some(self.config.max_skills),
        ) {
            Ok(plan) => plan,
            Err(message) => {
                row.status = Status::Failed;
                row.detail = "invalid_answer".into();
                let _ = self.finish(context, row, report, &message, &answer);
                return Err(Error::InvalidAnswer(message));
            }
        };
        report.dropped = plan.dropped.clone();
        report.retrospective = runlog::clean(&plan.retrospective, &redact);
        let skill_candidates = plan.skills.clone();
        let rejected = plan.rejected.clone();
        self.apply(context, &run_id, plan, &mut report, cancel)
            .await;
        if let Err(Error::Cancelled) = self
            .apply_skills(
                context,
                &run_id,
                shown,
                skill_candidates,
                cancel,
                &mut report,
            )
            .await
        {
            row.status = Status::Canceled;
            row.memories_proposed = report.candidates.len();
            report.items.extend(rejected);
            let _ = self.finish(context, row, report, "", "");
            return Err(Error::Cancelled);
        }

        row.status = Status::Ok;
        row.memories_proposed = report.candidates.len();
        row.memories_dropped = report.dropped.values().sum();
        row.skills_written = report.skills.len();
        report.status = Status::Ok;
        report.items.extend(rejected);
        self.finish(context, row, report, "", "")
    }

    /// Whether this run asks the model for skills: skills are on, there is a
    /// place to write them, and the slice is clean enough
    /// (`skill_source`). Records a withheld run on the report.
    pub(super) fn skills_allowed(
        &self,
        context: &Context<'_>,
        slice: &transcript::Slice,
        report: &mut Report,
    ) -> bool {
        if !self.config.skills {
            report.skills_note = "[reflection].skills is false";
            return false;
        }
        if context.skill_roots.is_none() || self.store.is_none() {
            report.skills_note = "skills are disabled ([skills].enabled) or there is no skill directory or reflection store";
            return false;
        }
        if slice.tainted() && self.config.skill_source == SkillSource::Untainted {
            report.skills_withheld = true;
            report.skills_note = "the slice has external entries and skill_source = \"untainted\"";
            return false;
        }
        true
    }

    /// The skills the model is shown: every discovered skill's name and
    /// description, and the body of the ones reflection owns, which are the
    /// only ones it may revise.
    pub(super) fn skill_context(&self, context: &Context<'_>) -> Vec<prompt::SkillInfo> {
        let Some(roots) = context.skill_roots else {
            return Vec::new();
        };
        context
            .runner
            .skills()
            .skills()
            .iter()
            .map(|skill| {
                let owned = self.owns(roots, &skill.name);
                let body = if owned {
                    crate::skill::load(skill).ok()
                } else {
                    None
                };
                let scripts = if owned {
                    skillwrite::scripts(roots, &skill.name).unwrap_or_default()
                } else {
                    Default::default()
                };
                prompt::SkillInfo {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    owned,
                    body,
                    scripts,
                }
            })
            .collect()
    }

    /// Whether reflection still owns `name`: it wrote it and no one has
    /// changed the file since.
    pub(super) fn owns(&self, roots: &skillwrite::Roots, name: &str) -> bool {
        let Some(row) = self.generated_skill(name) else {
            return false;
        };
        skillwrite::still_owned(roots, &row)
    }
}
