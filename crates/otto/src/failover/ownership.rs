//! Process-wide view of live session writer authority for signal handling.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use otto_session::SessionHandle;

#[derive(Default)]
pub(crate) struct WriterRegistry {
    sessions: Mutex<Vec<Weak<dyn SessionHandle>>>,
}

impl WriterRegistry {
    pub(crate) fn register(&self, session: &Arc<dyn SessionHandle>) {
        let mut registered = self.sessions.lock().expect("writer registry mutex");
        registered.retain(|session| session.strong_count() > 0);
        registered.push(Arc::downgrade(session));
    }

    pub(crate) fn holds_writer(&self) -> bool {
        let sessions = {
            let mut registered = self.sessions.lock().expect("writer registry mutex");
            registered.retain(|session| session.strong_count() > 0);
            registered
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        sessions
            .iter()
            .filter_map(|session| session.lease())
            .any(|ownership| ownership.check().is_ok())
    }
}

fn global() -> &'static WriterRegistry {
    static REGISTRY: OnceLock<WriterRegistry> = OnceLock::new();
    REGISTRY.get_or_init(WriterRegistry::default)
}

pub(crate) fn register_writer_session(session: &Arc<dyn SessionHandle>) {
    global().register(session);
}

pub fn holds_writer_ownership() -> bool {
    global().holds_writer()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::builder::MemoryHandle;
    use otto_core::session::Header;
    use otto_session::WriterOwnership;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct FakeOwnership(AtomicBool);

    impl WriterOwnership for FakeOwnership {
        fn check(&self) -> Result<(), String> {
            self.0
                .load(Ordering::SeqCst)
                .then_some(())
                .ok_or_else(|| "writer released".to_string())
        }

        fn release(&self) -> Result<(), String> {
            self.0.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn tracks_any_live_backend_writer_and_ignores_released_or_dropped_handles() {
        let registry = WriterRegistry::default();
        let ownership = Arc::new(FakeOwnership(AtomicBool::new(true)));
        let handle: Arc<dyn SessionHandle> = Arc::new(MemoryHandle::with_writer_ownership(
            Header::default(),
            ownership.clone() as Arc<dyn WriterOwnership>,
        ));
        registry.register(&handle);
        assert!(registry.holds_writer());

        ownership.release().expect("release");
        assert!(!registry.holds_writer());

        drop(handle);
        assert!(!registry.holds_writer());
    }

    #[test]
    fn register_drops_handles_that_no_longer_exist() {
        let registry = WriterRegistry::default();
        let dropped: Arc<dyn SessionHandle> = Arc::new(MemoryHandle::new(Header::default()));
        registry.register(&dropped);
        drop(dropped);
        let live: Arc<dyn SessionHandle> = Arc::new(MemoryHandle::new(Header::default()));
        registry.register(&live);
        assert_eq!(registry.sessions.lock().unwrap().len(), 1);
    }
}
