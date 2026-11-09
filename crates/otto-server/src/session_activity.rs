//! Session admission and exclusive execution ownership, separate from replay.

use super::*;

#[derive(Default)]
enum SessionActivity {
    #[default]
    Idle,
    Turn(Arc<Turn>),
    Compacting {
        generation: u64,
        cancel: CancellationToken,
    },
}

#[derive(Default)]
enum SessionAdmission {
    #[default]
    Open,
    Closing,
}

#[derive(Default)]
pub(super) struct SessionState {
    admission: SessionAdmission,
    activity: SessionActivity,
    last_turn: Option<Arc<Turn>>,
    compaction_generation: u64,
    /// Turns waiting for the running turn or compaction, oldest first. Every
    /// reader of this field holds the session lock, so the queue and activity
    /// change together.
    pub(super) queue: VecDeque<QueuedTurn>,
    /// Turns restored from the session's saved turn queue on open, oldest
    /// first. They join the front of `queue` once the recovery wake turn,
    /// if any, has run (see [`Server::release_restored`]).
    pub(super) restored: VecDeque<QueuedTurn>,
    pub(super) waiting: Option<WaitingApproval>,
    pub(super) decided: VecDeque<String>,
    /// The turns started for recent client `input_id`s, newest last.
    pub(super) inputs: VecDeque<(String, Arc<Turn>)>,
}

impl SessionState {
    pub(super) fn is_closing(&self) -> bool {
        matches!(self.admission, SessionAdmission::Closing)
    }

    pub(super) fn last_turn(&self) -> Option<&Arc<Turn>> {
        self.last_turn.as_ref()
    }

    pub(super) fn turn_running(&self) -> bool {
        matches!(self.activity, SessionActivity::Turn(_))
    }

    pub(super) fn is_compacting(&self) -> bool {
        matches!(self.activity, SessionActivity::Compacting { .. })
    }

    /// Queue handoff may claim idle ownership while more queued items remain.
    pub(super) fn begin_turn(&mut self, turn: Arc<Turn>) -> bool {
        if self.is_closing() || !matches!(self.activity, SessionActivity::Idle) {
            return false;
        }
        self.last_turn = Some(Arc::clone(&turn));
        self.activity = SessionActivity::Turn(turn);
        true
    }

    /// Call alongside turn settlement while holding the session mutex.
    pub(super) fn finish_turn(&mut self, turn: &Arc<Turn>) -> bool {
        if !matches!(&self.activity, SessionActivity::Turn(active) if Arc::ptr_eq(active, turn)) {
            return false;
        }
        self.activity = SessionActivity::Idle;
        true
    }

    pub(super) fn begin_compaction(&mut self, cancel: CancellationToken) -> Option<u64> {
        if self.is_closing() || self.busy() {
            return None;
        }
        self.compaction_generation = self.compaction_generation.checked_add(1)?;
        let generation = self.compaction_generation;
        self.activity = SessionActivity::Compacting { generation, cancel };
        Some(generation)
    }

    pub(super) fn finish_compaction(&mut self, generation: u64) -> bool {
        if !matches!(&self.activity, SessionActivity::Compacting { generation: active, .. } if *active == generation)
        {
            return false;
        }
        self.activity = SessionActivity::Idle;
        true
    }

    /// Seals admission permanently but retains ownership until work settles.
    pub(super) fn begin_close(&mut self) {
        self.admission = SessionAdmission::Closing;
        if let Some(turn) = &self.last_turn {
            turn.cancel();
        }
        if let SessionActivity::Compacting { cancel, .. } = &self.activity {
            cancel.cancel();
        }
    }

    /// Queued and restored turns reserve admission ahead of ordinary work.
    pub(crate) fn busy(&self) -> bool {
        self.busy_for_wake() || !self.restored.is_empty()
    }

    /// Recovery wakes must run before restored turns are released.
    pub(super) fn busy_for_wake(&self) -> bool {
        self.turn_running() || self.is_compacting() || !self.queue.is_empty()
    }

    pub(super) fn remember_decided(&mut self, id: &str) {
        if self.decided.iter().any(|known| known == id) {
            return;
        }
        if self.decided.len() == REMEMBERED_DECISIONS {
            self.decided.pop_front();
        }
        self.decided.push_back(id.to_string());
    }

    /// Ends every queued turn `canceled`; none of them ran. Their saved
    /// copies stay in the turn queue journal, so a later open of the
    /// session restores them; a caller that ends them for good also clears
    /// the journal.
    pub(super) fn cancel_queue(&mut self) {
        for queued in self.queue.drain(..).chain(self.restored.drain(..)) {
            queued.turn.finish(Some(String::new()), true);
        }
    }
}
