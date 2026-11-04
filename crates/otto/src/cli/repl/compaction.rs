//! `/compact` command.

use super::*;

impl<'a> Repl<'a> {
    /// Includes the checkpoint de-duplication that keeps an event and the
    /// returned result from rendering twice.
    pub(super) async fn compact(
        &mut self,
        focus: &str,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let Self {
            controller,
            stdout,
            stderr,
            program_status,
            delete_confirmation,
        } = self;
        *delete_confirmation = None;
        if let Some(reporter) = program_status {
            reporter.report(&mut **stdout, crate::program_status::State::Working);
        }
        let turn = cancel.child_token();
        let mut rendered_ids: Vec<String> = Vec::new();
        let mut rendered_noop_empty = false;
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| match &event {
                Event::CompactionCompleted { compaction } => {
                    if compaction.noop && compaction.checkpoint_id.is_empty() {
                        if rendered_noop_empty {
                            return;
                        }
                        rendered_noop_empty = true;
                    } else if !compaction.checkpoint_id.is_empty() {
                        if rendered_ids.contains(&compaction.checkpoint_id) {
                            return;
                        }
                        rendered_ids.push(compaction.checkpoint_id.clone());
                    }
                    let _ = write!(stdout, "{}", compaction_line(compaction));
                }
                _ => {
                    if render_event(&mut **stdout, &mut **stderr, &event) {
                        error_rendered = true;
                    }
                }
            };
            controller.compact(focus, &mut sink, &turn).await
        };
        if let Some(reporter) = program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let completion = if canceled {
                crate::program_status::Completion::Canceled
            } else {
                crate::program_status::Completion::from_result(&result)
            };
            reporter.report(&mut **stdout, completion.into());
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if error.is_cancelled() && !is_fatal_persistence(&error) {
                    return Ok(());
                }
                if !error_rendered {
                    let _ = writeln!(stderr, "{error}");
                }
                return Err(Error::Command {
                    command: "/compact".to_string(),
                    message: error.to_string(),
                });
            }
        };
        let already_rendered = if !result.checkpoint_id.is_empty() {
            rendered_ids.contains(&result.checkpoint_id)
        } else {
            result.noop && rendered_noop_empty
        };
        if !already_rendered {
            let _ = write!(stdout, "{}", compaction_line(&result));
        }
        Ok(())
    }

    pub(super) fn print_session_id(&mut self) {
        let id = self.controller.info().session_id;
        if !id.is_empty() {
            let _ = writeln!(self.stdout, "Session: {id}");
        }
    }
}

pub(super) fn compaction_line(result: &CompactionResult) -> String {
    if result.noop {
        return "\n[context] no-op\n".to_string();
    }
    match (result.tokens_before, result.estimated_tokens_after) {
        (Some(before), Some(after)) => format!(
            "\n[context] compacted {} \u{2192} {} tokens\n",
            format_token_count(before),
            format_token_count(after)
        ),
        (Some(before), None) => {
            format!(
                "\n[context] compacted {} tokens\n",
                format_token_count(before)
            )
        }
        (None, _) => "\n[context] compacted\n".into(),
    }
}

fn format_token_count(tokens: i64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    format!("{}k", tokens / 1000)
}
