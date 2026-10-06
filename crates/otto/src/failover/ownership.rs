//! Process-wide view of live session writer authority for signal handling.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::session::SessionHandle;

#[derive(Default)]
pub(crate) struct WriterRegistry {
    sessions: Mutex<Vec<Weak<dyn SessionHandle>>>,
}

impl WriterRegistry {
    pub(crate) fn register(&self, session: &Arc<dyn SessionHandle>) {
        self.sessions
            .lock()
            .expect("writer registry mutex")
            .push(Arc::downgrade(session));
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

pub fn register_writer_session(session: &Arc<dyn SessionHandle>) {
    global().register(session);
}

pub fn holds_writer_ownership() -> bool {
    global().holds_writer()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::builder::MemoryHandle;
    use crate::session::WriterOwnership;
    use otto_core::session::Header;
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
}
