//! Requested reflection: the `reflect` tool's port, run when the user asks
//! the model for a retrospective.
//!
//! It runs [`crate::reflection::Reflector::run`] with
//! [`Trigger::Requested`], so a requested run passes every check a `/reflect`
//! run does. Like background reflection, it runs beside the turn that asked
//! for it: it does not take the controller's admission, and it reads only
//! complete session records, so the slice ends at the user's request.
//!
//! Ownership: [`Controller::bind_reflection`] builds one [`Requested`] for
//! the current runner and session and binds it to the runner's
//! [`crate::tool::reflect::Handle`]. It holds the runner weakly, so the runner
//! and its tool do not keep each other alive. Every place that installs a
//! runner or a session calls `bind_reflection` again.
//!
//! Concurrency and cancellation: it shares the background single-flight flag,
//! so at most one requested or automatic run is active per controller; a call
//! while one runs returns at once and that run's result arrives as a notice.
//! The automatic cooldown does not apply. The turn's token and
//! `Controller::request_close` both cancel the run.
//!
//! Errors: a failed run is the tool's error text; a completed run also queues
//! its report line as a notice, so every frontend shows the same line the
//! model was given.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use tokio_util::sync::CancellationToken;

use super::Controller;
use super::auto_reflection::{Notices, Running};
use crate::memory::{Scope, Service};
use crate::reflection::skillwrite::Roots;
use crate::reflection::{self, Error, Report, Trigger};
use crate::runtime::builder::{Builder, Runner, SharedSession};

/// What the tool returns while another run holds the single-flight flag.
pub(super) const ALREADY_RUNNING: &str = "A reflection is already running for this session. Its result will appear as a reflection notice when it finishes; tell the user so and do not write a retrospective yourself.";

pub(super) struct Requested {
    builder: Arc<Builder>,
    runner: Weak<Runner>,
    session: SharedSession,
    memory: Option<(Arc<Service>, Scope, Scope)>,
    skill_roots: Option<Roots>,
    running: Arc<AtomicBool>,
    notices: Arc<Notices>,
    close: CancellationToken,
}

#[async_trait::async_trait]
impl crate::tool::reflect::Reflect for Requested {
    async fn reflect(&self, focus: &str, cancel: &CancellationToken) -> Result<String, String> {
        let reflector = &self.builder.reflector;
        if !reflector.enabled() {
            return Err(Error::Disabled.to_string());
        }
        let runner = self
            .runner
            .upgrade()
            .ok_or_else(|| super::CLOSED.to_owned())?;
        let (service, user_scope, workspace_scope) = self
            .memory
            .as_ref()
            .ok_or_else(|| Error::MemoryUnavailable.to_string())?;
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(ALREADY_RUNNING.to_owned());
        }
        let _running = Running(Arc::clone(&self.running));

        let token = self.close.child_token();
        let watcher = {
            let token = token.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                cancel.cancelled().await;
                token.cancel();
            })
        };
        let session_id = self.session.header().id;
        let session_path = self.session.path();
        let presentation_origin = runner.presentation_origin.get();
        let context = reflection::Context {
            approvals: Some(&self.builder.approvals),
            runner: &runner,
            session_id: &session_id,
            presentation_origin: presentation_origin.as_ref(),
            session_path: &session_path,
            service,
            user_scope,
            workspace_scope,
            skill_roots: self.skill_roots.as_ref(),
        };
        let result = reflector
            .run(&context, Trigger::Requested, focus, &token)
            .await;
        watcher.abort();
        match result {
            Ok(report) => {
                self.notices
                    .push_for(report.line(), presentation_origin.as_ref());
                Ok(model_text(&report))
            }
            Err(error) => Err(format!("reflection: {error}")),
        }
    }
}

/// What the model reads: the report line (which names the run log), the
/// retrospective, and how to relay them.
fn model_text(report: &Report) -> String {
    let mut text = report.summary();
    text.push_str(
        "\n\nGive the user the main points of the retrospective, the run log path, and what awaits their review: memory candidates in /memory review, skill changes as pending approvals.",
    );
    text
}

impl Controller {
    /// Binds the `reflect` tool of the current runner to the current
    /// session. Called whenever a runner or a session is installed.
    pub(super) fn bind_reflection(&self) {
        let Some((runner, session)) = self
            .lock()
            .current
            .as_ref()
            .map(|current| (Arc::clone(&current.runner), current.session.clone()))
        else {
            return;
        };
        runner.reflection.bind(Arc::new(Requested {
            builder: Arc::clone(&self.builder),
            runner: Arc::downgrade(&runner),
            session,
            memory: self.memory_manager(),
            skill_roots: self.reflection_skill_roots(),
            running: Arc::clone(&self.auto_running),
            notices: Arc::clone(&self.notices),
            close: self.auto_cancel.clone(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_text_carries_the_line_the_retrospective_and_the_relay_hint() {
        let mut report = Report::skipped_for_test();
        report.note.clear();
        report.retrospective = "Searched twice before reading the docs.".into();
        let text = model_text(&report);
        assert!(text.starts_with("reflection: nothing new to reflect on\n\nRetrospective:\nSearched twice before reading the docs.\n\n"), "{text}");
        assert!(text.contains("/memory review"));
        report.retrospective.clear();
        assert!(!model_text(&report).contains("Retrospective:"));
    }
}
