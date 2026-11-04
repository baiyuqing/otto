//! Observer requests use an independent ticket lane, never a command or surface.
use super::*;

pub(super) const OBSERVER_SURFACE: u64 = u64::MAX;
const OBSERVER_REFRESH: Duration = Duration::from_millis(300);

impl App {
    pub(crate) fn poll_observer(&mut self) {
        self.observer
            .reset_scope(&self.info.workspace, &self.info.session_id);
        if !self.observer.active
            || self.has_surface()
            || !self.connected
            || !self.session_changes.is_empty()
            || self
                .pending
                .iter()
                .any(|(surface, _)| *surface == OBSERVER_SURFACE)
            || self
                .observer_refreshed
                .is_some_and(|last| last.elapsed() < OBSERVER_REFRESH)
        {
            return;
        }
        self.observer_refreshed = Some(Instant::now());
        self.enqueue(OBSERVER_SURFACE, Request::ObserverSnapshot);
    }

    pub(super) fn complete_observer(&mut self, ticket: &Ticket, result: Result<Reply, String>) {
        if !self.pending.remove(&(ticket.surface, ticket.generation))
            || self.generations.get(&OBSERVER_SURFACE) != Some(&ticket.generation)
        {
            return;
        }
        match result {
            Ok(Reply::ObserverSnapshot(snapshot)) => self.observer.snapshot(snapshot),
            Err(error) => self.observer.error = Some(error),
            _ => {
                self.observer.error = Some("Invalid observer response; reconnect and retry".into())
            }
        }
    }
}
