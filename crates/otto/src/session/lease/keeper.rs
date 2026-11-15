//! The renewal and watchdog threads for the leases of one repository.

use super::stall::StallState;
use super::*;

/// Renews and watches the leases owned by one repository. Process fencing is
/// supplied by the failover service callback at composition.
pub(crate) struct Keeper {
    pub(super) state: Mutex<Vec<Weak<Lease>>>,
    pub(super) threads_started: Mutex<bool>,
    pub(super) on_fence: Box<dyn Fn() + Send + Sync>,
    /// How often the renewal and watchdog threads wake to check their
    /// leases. Production leaves this at one second; a test keeper can
    /// shorten it so a small `L` does not have to wait out a full
    /// production tick to see renewal or fencing happen.
    pub(super) tick_interval: Duration,
    /// The watchdog's schedule and the grace period; see `stall.rs`.
    pub(super) stall: Mutex<StallState>,
}

impl Keeper {
    pub(crate) fn new(
        on_fence: Box<dyn Fn() + Send + Sync>,
        tick_interval: Duration,
    ) -> Arc<Keeper> {
        Arc::new(Keeper {
            state: Mutex::new(Vec::new()),
            threads_started: Mutex::new(false),
            on_fence,
            tick_interval,
            stall: Mutex::new(StallState::default()),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        on_fence: Box<dyn Fn() + Send + Sync>,
        children: Arc<Children>,
        tick_interval: Duration,
    ) -> Arc<Keeper> {
        Keeper::new(
            Box::new(move || {
                children.kill_all();
                on_fence();
            }),
            tick_interval,
        )
    }

    pub(super) fn register(keeper: &Arc<Keeper>, lease: &Arc<Lease>) {
        keeper.state.lock().unwrap().push(Arc::downgrade(lease));
        Keeper::ensure_threads(keeper);
    }

    fn ensure_threads(keeper: &Arc<Keeper>) {
        let mut started = keeper.threads_started.lock().unwrap();
        if *started {
            return;
        }
        *started = true;
        let renewal = Arc::downgrade(keeper);
        std::thread::Builder::new()
            .name("otto-lease-renewal".into())
            .spawn(move || Keeper::renewal_loop(renewal))
            .expect("spawn lease renewal thread");
        let watchdog = Arc::downgrade(keeper);
        std::thread::Builder::new()
            .name("otto-lease-watchdog".into())
            .spawn(move || Keeper::watchdog_loop(watchdog))
            .expect("spawn lease watchdog thread");
    }

    fn renewal_loop(keeper: Weak<Keeper>) {
        loop {
            let Some(interval) = keeper.upgrade().map(|k| k.tick_interval) else {
                return;
            };
            std::thread::sleep(interval);
            let Some(keeper) = keeper.upgrade() else {
                return;
            };
            for lease in keeper.live_leases() {
                lease.maybe_renew();
            }
        }
    }

    fn watchdog_loop(keeper: Weak<Keeper>) {
        if let Some(keeper) = keeper.upgrade() {
            keeper.record_watchdog_tick();
        }
        loop {
            let Some(interval) = keeper.upgrade().map(|k| k.tick_interval) else {
                return;
            };
            std::thread::sleep(interval);
            let Some(keeper) = keeper.upgrade() else {
                return;
            };
            keeper.record_watchdog_tick();
            for lease in keeper.live_leases() {
                if let Some(reason) = lease.should_fence(false) {
                    keeper.fence(&lease, reason);
                }
            }
        }
    }

    fn live_leases(&self) -> Vec<Arc<Lease>> {
        let mut state = self.state.lock().unwrap();
        let mut live = Vec::new();
        state.retain(|weak| match weak.upgrade() {
            Some(lease) => {
                live.push(lease);
                true
            }
            None => false,
        });
        live
    }

    fn fence(&self, lease: &Arc<Lease>, reason: String) {
        if lease.fenced.swap(true, Ordering::AcqRel) {
            return;
        }
        lease.set_lost_reason(reason);
        (self.on_fence)();
    }

    /// True when at least one registered lease is still live (its `Arc` has
    /// not been dropped) and neither released, superseded, nor fenced: this
    /// process is still the acknowledged holder of at least one session.
    #[cfg(test)]
    pub(super) fn holds_any(&self) -> bool {
        self.live_leases().iter().any(|lease| {
            !lease.released.load(Ordering::Acquire)
                && !lease.superseded.load(Ordering::Acquire)
                && !lease.fenced.load(Ordering::Acquire)
        })
    }
}
