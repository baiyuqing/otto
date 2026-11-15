//! The session repository over the POSIX journal files: [`PosixSessionRepository`].

use super::{
    ArchiveResult, Forked, Prepared, Recovery, Store, fork, inspect, list, list_for_delete,
    prepared, session_directory,
};
use std::path::Path;
use std::sync::Arc;

use super::lease::Keeper;
use crate::fsops::files::FileDriver;
use crate::session::{PreparedSession, SessionHandle, SessionRepository};
use otto_core::session::{Header, ListResult, SessionInfo, Warning};

impl SessionHandle for Store {
    fn append_input_submissions(&self, encoded: &str) -> Result<(), String> {
        self.append_custom_entry("otto.input_submissions", encoded)
            .map_err(|error| error.to_string())
    }
    fn input_submission_events(&self) -> Result<Vec<String>, String> {
        let state = self.state.lock().map_err(|_| "session lock poisoned")?;
        let (index, _) = otto_core::session::context::index_context_entries(&state.entries)
            .map_err(|error| error.to_string())?;
        let path = otto_core::session::context::active_context_path(
            &state.entries,
            state.leaf_id.as_deref().unwrap_or(""),
            &index,
        )
        .map_err(|error| error.to_string())?;
        Ok(path
            .iter()
            .filter_map(|entry| {
                let custom = entry.custom.as_ref()?;
                (custom.custom_type == "otto.input_submissions")
                    .then(|| custom.data.as_ref().map(|data| data.get().to_owned()))
                    .flatten()
            })
            .collect())
    }
    fn append_local_command(&self, encoded: &str) -> Result<(), String> {
        self.append_custom_entry(
            otto_core::session::local_command::LOCAL_COMMAND_CUSTOM_TYPE,
            encoded,
        )
        .map_err(|error| error.to_string())
    }
    fn local_commands(&self) -> Vec<otto_core::session::local_command::LocalCommandReceipt> {
        use otto_core::session::local_command::{LOCAL_COMMAND_CUSTOM_TYPE, fold_local_commands};
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let Ok((index, _)) = otto_core::session::context::index_context_entries(&state.entries)
        else {
            return Vec::new();
        };
        let Ok(path) = otto_core::session::context::active_context_path(
            &state.entries,
            state.leaf_id.as_deref().unwrap_or(""),
            &index,
        ) else {
            return Vec::new();
        };
        fold_local_commands(path.iter().filter_map(|entry| {
            let custom = entry.custom.as_ref()?;
            if custom.custom_type != LOCAL_COMMAND_CUSTOM_TYPE {
                return None;
            }
            serde_json::from_str(custom.data.as_ref()?.get()).ok()
        }))
    }

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
    fn update_thinking_level(&self, value: &str) -> Result<(), String> {
        Store::update_thinking_level(self, value).map_err(|error| error.to_string())
    }
    fn update_runtime(&self, runtime: &otto_core::session::RuntimeMetadata) -> Result<(), String> {
        Store::update_runtime(self, runtime).map_err(|error| error.to_string())
    }
    fn close(&self) -> Result<(), String> {
        Store::close(self).map_err(|error| error.to_string())
    }
    fn permanent_delete(&self, root: &Path, workspace: &str) -> Result<(), String> {
        self.delete_permanently(root, workspace)
            .map_err(|error| error.to_string())
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
    fn append_custom_when_created<'a>(
        &'a self,
        custom_type: &'a str,
        data: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), otto_core::session::SessionError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Store::append_custom_entry_when_created(self, custom_type, data)
                .map_err(|error| otto_core::session::SessionError::Persist(error.to_string()))
        })
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
    fn list_for_delete(
        &self,
        root: &Path,
        workspace: &str,
    ) -> Result<Vec<crate::session::DeleteCandidate>, String> {
        list_for_delete::list_for_delete(root, workspace).map_err(|error| error.to_string())
    }

    fn permanent_delete(
        &self,
        root: &Path,
        workspace: &str,
        reference: &str,
    ) -> Result<(), String> {
        Prepared::prepare_delete(
            root,
            workspace,
            Path::new(reference),
            self.driver.clone(),
            self.keeper.clone(),
        )
        .and_then(|prepared| prepared.delete_permanently(root, workspace))
        .map_err(|error| error.to_string())
    }
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
        request_id: Option<&str>,
    ) -> Result<Forked, otto_core::session::ForkError> {
        fork::fork_with_request(
            self.driver.as_ref(),
            Path::new(reference),
            before,
            new_id,
            request_id,
        )
    }

    fn fork_points(&self, reference: &str) -> Result<Vec<otto_core::session::ForkPoint>, String> {
        let (entries, _) = Store::read_entries(reference).map_err(|error| error.to_string())?;
        let leaf = entries
            .last()
            .map(|entry| entry.id.as_str())
            .unwrap_or_default();
        otto_core::session::fork_points(&entries, leaf).map_err(|error| error.to_string())
    }

    fn fork_context(
        &self,
        reference: &str,
        prompt: &str,
    ) -> Result<Vec<otto_core::model::Message>, String> {
        fork::fork_context(reference, prompt)
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
        let path = Path::new(reference);
        let prepared = Prepared::prepare_listed_with_driver_and_keeper(
            root,
            workspace,
            path,
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
        let path = Path::new(reference);
        prepared::archive_with_driver_and_keeper(
            root,
            workspace,
            path,
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
