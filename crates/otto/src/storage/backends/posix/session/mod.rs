//! POSIX session journal repository and its stable native façade.

mod fork;
mod list;
pub(crate) mod prepared;
pub(super) mod store;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use crate::session::fsops;
pub use fork::Forked;
pub use list::{MAX_LIST_SESSIONS, inspect, list, session_directory};
pub use prepared::{ArchiveResult, Prepared};
pub(crate) use store::unanswered_calls_from;
pub use store::{Recovery, RecoveryCause, Store, UnansweredCall};

use std::path::Path;
use std::sync::Arc;

use crate::session::{PreparedSession, SessionHandle, SessionRepository};
use crate::storage::backends::posix::lease::Keeper;
use crate::storage::files::FileDriver;
use otto_core::session::{Header, ListResult, SessionInfo, Warning};

const MAX_VALIDATED_SESSION_ROOT_ENTRIES: usize = 10_000;

impl SessionHandle for Store {
    fn custom_data(&self, custom_type: &str) -> Option<String> {
        let state = self.state.lock().ok()?;
        let (index, _) = otto_core::session::context::index_context_entries(&state.entries).ok()?;
        let path = otto_core::session::context::active_context_path(
            &state.entries,
            state.leaf_id.as_deref().unwrap_or(""),
            &index,
        )
        .ok()?;
        path.iter().rev().find_map(|entry| {
            let custom = entry.custom.as_ref()?;
            (custom.custom_type == custom_type)
                .then(|| custom.data.as_ref().map(|data| data.get().to_owned()))
                .flatten()
        })
    }
    fn header(&self) -> Header {
        Store::header(self)
    }
    fn name(&self) -> String {
        Store::name(self)
    }
    fn path(&self) -> String {
        Store::path(self)
    }
    fn rename(&self, name: &str) -> Result<(), String> {
        Store::rename(self, name).map_err(|error| error.to_string())
    }
    fn thinking_level(&self) -> String {
        Store::thinking_level(self)
    }
    fn update_thinking_level(&self, value: &str) -> Result<(), String> {
        Store::update_thinking_level(self, value).map_err(|error| error.to_string())
    }
    fn update_runtime(&self, runtime: &otto_core::session::RuntimeMetadata) -> Result<(), String> {
        Store::update_runtime(self, runtime).map_err(|error| error.to_string())
    }
    fn close(&self) -> Result<(), String> {
        Store::close(self).map_err(|error| error.to_string())
    }
    fn archive(&self, root: &Path, workspace: &str) -> Result<ArchiveResult, String> {
        Store::archive(self, root, workspace).map_err(|error| error.to_string())
    }
    fn snapshot(&self) -> otto_core::session::Snapshot {
        Store::snapshot(self)
    }
    fn lease(&self) -> Option<Arc<dyn crate::session::WriterOwnership>> {
        Store::lease(self)
    }
    fn take_recovery(&self) -> Option<Recovery> {
        Store::take_recovery(self)
    }
    fn forked_from(&self) -> Option<otto_core::session::ForkRecord> {
        Store::forked_from(self)
    }
}

/// Session repository backed by the injected confined file driver.
pub struct PosixSessionRepository {
    driver: Arc<dyn FileDriver>,
    keeper: Arc<Keeper>,
}

impl PosixSessionRepository {
    pub(crate) fn new(driver: Arc<dyn FileDriver>, keeper: Arc<Keeper>) -> Self {
        Self { driver, keeper }
    }
}

impl SessionRepository for PosixSessionRepository {
    fn custom_data_for(
        &self,
        root: &Path,
        workspace: &str,
        session_id: &str,
        custom_type: &str,
    ) -> Option<String> {
        if !crate::session::is_session_id(session_id) {
            return None;
        }
        let path = session_directory(root, workspace)
            .ok()?
            .join(format!("{session_id}.jsonl"));
        let (info, _) = inspect(&path).ok()?;
        if info.id != session_id {
            return None;
        }
        self.custom_data(&path.to_string_lossy(), custom_type)
    }
    fn custom_data(&self, reference: &str, custom_type: &str) -> Option<String> {
        let (entries, _) = Store::read_entries(reference).ok()?;
        let (index, _) = otto_core::session::context::index_context_entries(&entries).ok()?;
        let leaf = entries.last().map_or("", |entry| entry.id.as_str());
        let path = otto_core::session::context::active_context_path(&entries, leaf, &index).ok()?;
        path.iter().rev().find_map(|entry| {
            let custom = entry.custom.as_ref()?;
            (custom.custom_type == custom_type)
                .then(|| custom.data.as_ref().map(|data| data.get().to_owned()))
                .flatten()
        })
    }
    fn fork(
        &self,
        reference: &str,
        before: Option<&str>,
        new_id: &str,
    ) -> Result<Forked, otto_core::session::ForkError> {
        fork::fork(self.driver.as_ref(), Path::new(reference), before, new_id)
    }

    fn fork_points(&self, reference: &str) -> Result<Vec<otto_core::session::ForkPoint>, String> {
        let (entries, _) = Store::read_entries(reference).map_err(|error| error.to_string())?;
        let leaf = entries
            .last()
            .map(|entry| entry.id.as_str())
            .unwrap_or_default();
        otto_core::session::fork_points(&entries, leaf).map_err(|error| error.to_string())
    }

    fn validate_existing_root(&self, root: &Path) -> Result<(), String> {
        let root_handle = self
            .driver
            .open_root(root)
            .map_err(|error| format!("open session root: {error}"))?;
        let names = root_handle
            .list_names(MAX_VALIDATED_SESSION_ROOT_ENTRIES)
            .map_err(|error| format!("read session root: {error}"))?;
        if names.iter().any(|name| name == ".dataset-sessions.json") {
            crate::storage::backends::posix::journal_migration::read_dataset_alias(root, "")
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        for name in names {
            match root_handle.open_directory(&name) {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(error) => return Err(format!("open session namespace: {error}")),
            }
        }
        Err("configured session root has no existing namespace".into())
    }

    fn create_lazy(
        &self,
        root: &Path,
        header: Header,
        failover_seconds: Option<u64>,
    ) -> Result<Arc<dyn SessionHandle>, String> {
        let store = Store::create_lazy_with_driver_and_keeper(
            root,
            header,
            Arc::clone(&self.driver),
            Arc::clone(&self.keeper),
        )
        .map_err(|error| error.to_string())?;
        if let Some(seconds) = failover_seconds {
            store
                .enable_failover(seconds)
                .map_err(|error| error.to_string())?;
        }
        Ok(Arc::new(store))
    }

    fn prepare(
        &self,
        reference: &str,
        create_lease: Option<u64>,
    ) -> Result<Box<dyn PreparedSession>, String> {
        let prepared = Prepared::prepare_with_driver_and_keeper(
            Path::new(reference),
            create_lease,
            Arc::clone(&self.driver),
            Arc::clone(&self.keeper),
        )
        .map_err(|error| error.to_string())?;
        Ok(Box::new(prepared))
    }

    fn prepare_in(
        &self,
        root: &Path,
        reference: &str,
        create_lease: Option<u64>,
    ) -> Result<Box<dyn PreparedSession>, String> {
        let resolved =
            crate::storage::backends::posix::journal_migration::read_dataset_alias(root, reference)
                .map_err(|error| error.to_string())?;
        match resolved {
            Some(relative) => {
                let prepared = Prepared::prepare_with_driver_and_keeper(
                    &root.join(relative),
                    create_lease,
                    Arc::clone(&self.driver),
                    Arc::clone(&self.keeper),
                )
                .map_err(|error| error.to_string())?;
                Ok(Box::new(prepared))
            }
            None => self.prepare(reference, create_lease),
        }
    }

    fn create_child_lazy(
        &self,
        parent_reference: &str,
        name: &str,
        header: Header,
        parent_writer: Option<Arc<dyn crate::session::WriterOwnership>>,
    ) -> Result<(Arc<dyn SessionHandle>, String), String> {
        let store = Store::create_child_lazy_with_driver_and_keeper(
            Path::new(parent_reference),
            name,
            header,
            Arc::clone(&self.driver),
            Arc::clone(&self.keeper),
        )
        .map_err(|error| error.to_string())?;
        if let Some(parent_writer) = parent_writer {
            store.set_lease_check(parent_writer);
        }
        let reference = Store::child_path(Path::new(parent_reference), name)
            .to_string_lossy()
            .into_owned();
        Ok((Arc::new(store), reference))
    }

    fn prepare_listed(
        &self,
        root: &Path,
        workspace: &str,
        reference: &str,
        create_lease: Option<u64>,
    ) -> Result<Box<dyn PreparedSession>, String> {
        let resolved =
            crate::storage::backends::posix::journal_migration::read_dataset_alias(root, reference)
                .map_err(|error| error.to_string())?;
        let path = resolved.map_or_else(
            || Path::new(reference).to_path_buf(),
            |relative| root.join(relative),
        );
        let prepared = Prepared::prepare_listed_with_driver_and_keeper(
            root,
            workspace,
            &path,
            create_lease,
            Arc::clone(&self.driver),
            Arc::clone(&self.keeper),
        )
        .map_err(|error| error.to_string())?;
        Ok(Box::new(prepared))
    }

    fn list(
        &self,
        root: &Path,
        workspace: &str,
        current_reference: &str,
        limit: usize,
    ) -> Result<ListResult, String> {
        list::list(root, workspace, current_reference, limit).map_err(|error| error.to_string())
    }

    fn archive(
        &self,
        root: &Path,
        workspace: &str,
        reference: &str,
    ) -> Result<ArchiveResult, String> {
        let resolved =
            crate::storage::backends::posix::journal_migration::read_dataset_alias(root, reference)
                .map_err(|error| error.to_string())?;
        let path = resolved.map_or_else(
            || Path::new(reference).to_path_buf(),
            |relative| root.join(relative),
        );
        prepared::archive_with_driver_and_keeper(
            root,
            workspace,
            &path,
            Arc::clone(&self.driver),
            Arc::clone(&self.keeper),
        )
        .map_err(|error| error.to_string())
    }
}

impl PreparedSession for Prepared {
    fn info(&self) -> SessionInfo {
        Prepared::info(self)
    }

    fn with_replayable(self: Box<Self>, replayable: fn(&str) -> bool) -> Box<dyn PreparedSession> {
        Box::new(Prepared::with_replayable(*self, replayable))
    }

    fn activate(self: Box<Self>) -> Result<(Arc<dyn SessionHandle>, Vec<Warning>), String> {
        let (store, warnings) = Prepared::activate(*self).map_err(|error| error.to_string())?;
        Ok((Arc::new(store), warnings))
    }

    fn close(self: Box<Self>) -> Result<(), String> {
        Prepared::close(&self).map_err(|error| error.to_string())
    }
}
