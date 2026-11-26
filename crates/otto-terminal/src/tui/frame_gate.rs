//! Frame pacing shared by local and attached terminal event loops.

use std::time::{Duration, Instant};

/// Maximum cadence for bursty visible updates such as streamed text.
pub(super) const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How one event affects the visible terminal snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Redraw {
    None,
    Paced,
    Immediate,
}

/// Tracks visible work that may wait until the next terminal frame.
#[derive(Debug)]
pub(super) struct FrameGate {
    dirty: bool,
    last_draw: Instant,
}

impl FrameGate {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            dirty: false,
            last_draw: now,
        }
    }

    /// Records a visible update and returns true when it may draw immediately.
    pub(super) fn request(&mut self, now: Instant) -> bool {
        self.dirty = true;
        now.duration_since(self.last_draw) >= FRAME_INTERVAL
    }

    /// The deadline for a pending paced frame.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.dirty.then(|| self.last_draw + FRAME_INTERVAL)
    }

    /// Records that the current visible snapshot was drawn.
    pub(super) fn drew(&mut self, now: Instant) {
        self.dirty = false;
        self.last_draw = now;
    }
}

/// Waits for a paced frame only while one is pending.
pub(super) fn wire_redraw(event: &otto_core::wire::events::WireEvent) -> Redraw {
    match event.event_type.as_str() {
        "text_delta" | "reasoning_delta" => Redraw::Paced,
        _ => Redraw::Immediate,
    }
}

pub(super) async fn wait_for_frame(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursty_updates_share_the_next_frame() {
        let start = Instant::now();
        let mut gate = FrameGate::new(start);

        assert!(!gate.request(start + Duration::from_millis(1)));
        assert!(!gate.request(start + Duration::from_millis(8)));
        assert_eq!(gate.deadline(), Some(start + FRAME_INTERVAL));

        let drawn_at = start + FRAME_INTERVAL + Duration::from_millis(5);
        gate.drew(drawn_at);
        assert_eq!(gate.deadline(), None);

        assert!(
            !gate.request(drawn_at + Duration::from_millis(1)),
            "an update just after a draw must wait for the next frame"
        );
        assert_eq!(gate.deadline(), Some(drawn_at + FRAME_INTERVAL));
    }

    #[test]
    fn wire_events_pace_only_streamed_content() {
        let mut event = otto_core::wire::events::WireEvent {
            event_type: "text_delta".into(),
            ..Default::default()
        };
        assert_eq!(wire_redraw(&event), Redraw::Paced);
        event.event_type = "reasoning_delta".into();
        assert_eq!(wire_redraw(&event), Redraw::Paced);
        event.event_type = "tool_call_started".into();
        assert_eq!(wire_redraw(&event), Redraw::Immediate);
    }

    #[test]
    fn update_after_frame_interval_can_draw_immediately() {
        let start = Instant::now();
        let mut gate = FrameGate::new(start);

        assert!(gate.request(start + FRAME_INTERVAL));
        gate.drew(start + FRAME_INTERVAL);
        assert_eq!(gate.deadline(), None);
    }
}
