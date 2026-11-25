//! Foreground image turn execution and preflight.
use super::*;

impl Runner {
    pub async fn run_with_image_system_context(
        &self,
        user_text: &str,
        image: Block,
        system_context: &str,
        scope: BudgetScope,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.run_with_images_system_context(
            user_text,
            vec![image],
            system_context,
            scope,
            emit,
            cancel,
        )
        .await
    }

    /// Checks typed image attachments before admission without side effects.
    pub fn validate_images(&self, images: &[Block]) -> Result<(), AgentError> {
        self.agent.validate_images(images)
    }

    pub async fn run_with_images_system_context(
        &self,
        user_text: &str,
        images: Vec<Block>,
        system_context: &str,
        scope: BudgetScope,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.validate_images(&images)?;
        let _output_lease = self.retain_temporary_outputs();
        let control = Control::new(self.turn_deadline());
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let mut send = |event| {
            let _ = events.send(event);
        };
        let mut collecting = self.collecting(&mut send);
        let run = self.agent.run_with_images_budget(
            scope,
            user_text,
            images,
            system_context,
            &mut collecting,
            &control,
        );
        let result = self
            .monitor_approvals(
                drive_with_control(run, cancel, &control),
                received,
                &mut *emit,
            )
            .await;
        self.report_lost_steering(&mut *emit).await;
        result
    }
}
