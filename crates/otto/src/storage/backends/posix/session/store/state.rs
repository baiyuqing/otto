//! `StoreState` methods that run under the store lock.

use super::*;

impl StoreState {
    /// [`Store::append_message`] under the store lock.
    pub(super) fn append_message(&mut self, message: &Message) -> Result<(), PiError> {
        let state = self;
        state.writable()?;
        message
            .validate()
            .map_err(|error| PiError::invalid(error.0))?;
        let recorded = recording::RECORD_REDACTOR.redact_record_message(message);
        let message = &recorded;
        state.ensure_file_fatal()?;

        let entry_id = state.new_entry_id("session")?;
        let (entry, persisted) =
            model_message_to_pi_entry(message, &entry_id, state.leaf_id.as_deref(), &state.header)?;
        let mut candidate = state.messages.clone();
        candidate.push(persisted.clone());
        pending_tool_calls(&candidate)?;
        state.append_entry(entry, entry_id)?;

        if persisted.role == Role::Assistant && persisted.usage.is_some() {
            state.aggregate_usage =
                add_resolved_usage(state.aggregate_usage, persisted.usage.as_ref());
            state.usage_present = true;
        }
        if let Some(compaction) = state.latest_compaction.as_mut()
            && compaction.first_post_checkpoint_message_id.is_empty()
        {
            compaction.first_post_checkpoint_message_id = persisted.id.clone();
        }
        if !persisted.input_id.is_empty() {
            state.input_ids.insert(persisted.input_id.clone());
        }
        state.messages.push(persisted);
        Ok(())
    }
}

impl StoreState {
    pub(super) fn append_custom_entry_unchecked(
        &mut self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), PiError> {
        self.ensure_file_fatal()?;
        let timestamp = format_persisted_timestamp(Utc::now(), "custom entry")?;
        let entry_id = self.new_entry_id("custom")?;
        let mut entry = PiEntry::new("custom", &entry_id, self.leaf_id.clone(), &timestamp);
        entry.custom = Some(PiCustom {
            custom_type: custom_type.to_owned(),
            data: Some(raw_value(recording::custom_data(custom_type, data))?),
        });
        self.append_entry(entry, entry_id)
    }

    /// Rejects a write on a closed or poisoned store.
    pub(crate) fn writable(&self) -> Result<(), PiError> {
        if self.closed {
            return Err(PiError::closed());
        }
        if let Some(fatal) = self.fatal.as_ref() {
            return Err(fatal.clone());
        }
        Ok(())
    }

    /// Creates the directory, file, header, runtime entry and the queued
    /// deferred custom entries, in that order, in one published file. A no-op
    /// once the file exists.
    pub(crate) fn ensure_file(&mut self) -> Result<(), PiError> {
        if self.file.is_some() {
            return Ok(());
        }
        let (directory, path) = match &self.file_path {
            Some(path) => (
                path.parent().unwrap_or(Path::new("")).to_path_buf(),
                path.clone(),
            ),
            None => {
                let directory =
                    super::super::list::session_directory(&self.root, &self.header.workspace)?;
                let path = directory.join(format!("{}.jsonl", self.header.id));
                (directory, path)
            }
        };

        let directory_name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PiError::invalid("session directory name is not a valid component"))?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PiError::invalid("session file name is not a valid component"))?;
        let root_path = directory
            .parent()
            .ok_or_else(|| PiError::invalid("session directory has no parent"))?;
        let root = open_or_create_root(self.file_driver.0.as_ref(), root_path)
            .map_err(|error| PiError::other(format!("open session root: {error}")))?;
        let session_dir = match root.open_directory(directory_name) {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                root.create_directory(directory_name).map_err(|error| {
                    PiError::other(format!("create session directory: {error}"))
                })?;
                root.sync_all()
                    .map_err(|error| PiError::other(format!("sync session root: {error}")))?;
                root.open_directory(directory_name)
                    .map_err(|error| PiError::other(format!("open session directory: {error}")))?
            }
            Err(error) => {
                return Err(PiError::other(format!("open session directory: {error}")));
            }
        };

        // A lazy, top-level store `Store::enable_failover` marked: acquire its
        // own lease directory before the session file exists, so the file is
        // never written without lease protection. A child transcript never
        // has `enable_failover` set (see `Store::create_child_lazy`). The
        // session directory must exist first: the lease directory is created
        // beside the (not yet written) session file.
        if let Some(lease_seconds) = self.enable_failover.take() {
            lease::create_lease_dir(&path, lease_seconds).map_err(|error| {
                PiError::other(format!("create session lease directory: {error}"))
            })?;
            let (lease, _acquired) = lease::Lease::acquire_with_keeper(&path, &self.keeper.0)
                .map_err(|error| PiError::other(format!("acquire session lease: {error}")))?;
            self.writer_ownership =
                Some(Arc::clone(&lease) as Arc<dyn crate::session::WriterOwnership>);
            self.lease = Some(lease);
            self.owns_lease = true;
        }

        // The suffix is not `.jsonl`: session scans read only `*.jsonl`.
        let temp_name = format!(
            "{filename}.{}.tmp",
            crate::auth::random_hex::<8>()
                .map_err(|error| PiError::other(format!("name session file: {error}")))?
        );
        let mut file = session_dir
            .open_journal(&temp_name, JournalOpen::CreateNew)
            .map_err(|error| PiError::other(format!("create session file: {error}")))?;

        let mut published = false;
        let result = file
            .lock_exclusive()
            .map_err(|error| PiError::other(format!("lock session file: {error}")))
            .and_then(|_| self.check_lease())
            .and_then(|_| self.write_initial_records(file.as_mut()))
            .and_then(|value| {
                // A process exit before the publish leaves only the temporary
                // file, which no session scan reads.
                session_dir
                    .publish_journal(&temp_name, filename)
                    .map_err(|error| PiError::other(format!("publish session file: {error}")))?;
                published = true;
                session_dir
                    .sync_all()
                    .map(|_| value)
                    .map_err(|error| PiError::other(format!("sync session directory: {error}")))
            });
        let (file_bytes, written_entries) = match result {
            Ok(value) => value,
            Err(error) => {
                drop(file);
                let held = if published { filename } else { &temp_name };
                if let Err(remove) = session_dir.remove_journal(held)
                    && remove.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(PiError::other(format!(
                        "{error}; remove incomplete session file: {remove}"
                    )));
                }
                return Err(error);
            }
        };

        self.path = path.to_string_lossy().into_owned();
        self.leaf_id = written_entries.last().map(|entry| entry.id.clone());
        self.entry_ids = written_entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        self.entries = written_entries;
        self.deferred_custom_entries.clear();
        self.file = Some(file);
        self.file_bytes = file_bytes;
        Ok(())
    }

    pub(super) fn write_initial_records(
        &self,
        file: &mut dyn JournalFile,
    ) -> Result<(i64, Vec<PiEntry>), PiError> {
        use otto_core::session::pi::{PI_SESSION_VERSION, PiHeader};
        let timestamp = format_rfc3339_nano(self.header.created_at);
        let header = PiHeader {
            type_name: "session".into(),
            version: PI_SESSION_VERSION,
            id: self.header.id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.header.workspace.clone(),
            parent_session: self.parent_session.clone(),
            raw: Vec::new(),
        };
        let mut file_bytes = write_pi_record(file, &encode_pi_record(PiRecord::Header(&header))?)
            .map_err(|error| error.context("write session header"))?;

        let runtime_id = fsops::new_pi_entry_id(&HashSet::new())
            .map_err(|error| error.context("generate runtime entry id"))?;
        let runtime_json = serde_json::to_string(&RuntimeMetadata {
            profile: self.header.profile.clone(),
            provider: self.header.provider.clone(),
            model: self.header.model.clone(),
        })
        .map_err(|error| PiError::other(format!("encode runtime metadata: {error}")))?;
        let mut entry = PiEntry::new("custom", &runtime_id, None, &timestamp);
        entry.custom = Some(PiCustom {
            custom_type: OTTO_RUNTIME_CUSTOM_TYPE.to_owned(),
            data: Some(raw_value(runtime_json)?),
        });
        file_bytes += write_pi_record(file, &encode_pi_record(PiRecord::Entry(&entry))?)
            .map_err(|error| error.context("write runtime metadata"))?;
        let mut written_entries = vec![entry];
        for deferred in &self.deferred_custom_entries {
            let used_ids: HashSet<String> = written_entries
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            let deferred_id = fsops::new_pi_entry_id(&used_ids)
                .map_err(|error| error.context("generate deferred custom entry id"))?;
            let parent_id = written_entries.last().map(|entry| entry.id.clone());
            let mut deferred_entry = PiEntry::new("custom", &deferred_id, parent_id, &timestamp);
            deferred_entry.custom = Some(PiCustom {
                custom_type: deferred.custom_type.clone(),
                data: Some(deferred.data.clone()),
            });
            file_bytes +=
                write_pi_record(file, &encode_pi_record(PiRecord::Entry(&deferred_entry))?)
                    .map_err(|error| error.context("write deferred custom entry"))?;
            written_entries.push(deferred_entry);
        }
        Ok((file_bytes, written_entries))
    }

    /// Creates the file, poisoning the store when creation fails.
    pub(crate) fn ensure_file_fatal(&mut self) -> Result<(), PiError> {
        if let Err(error) = self.ensure_file() {
            let fatal = PiError::fatal(error);
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        }
        Ok(())
    }

    /// Resolves the entry a new checkpoint may anchor to.
    ///
    /// With a retained-tail checkpoint in force, only a real context entry
    /// that comes after that checkpoint on the active path is accepted.
    /// Otherwise any entry on the active path is.
    pub(crate) fn compaction_anchor(&self, requested: &str) -> Result<String, PiError> {
        let (index, _) = index_context_entries(&self.entries)?;
        let leaf = self.leaf_id.clone().unwrap_or_default();
        let path = active_context_path(&self.entries, &leaf, &index)?;

        if let Some(active) = self
            .latest_compaction
            .as_ref()
            .filter(|compaction| compaction.retained_tail_only)
        {
            let checkpoint = path
                .iter()
                .position(|entry| entry.id == active.id && entry.type_name == "compaction")
                .ok_or_else(|| {
                    PiError::invalid("active retained-tail checkpoint is not on the active path")
                })?;
            for entry in &path[checkpoint + 1..] {
                if entry.id != requested {
                    continue;
                }
                if !is_real_compaction_context_entry(entry) {
                    return Err(PiError::invalid(
                        "retained-tail anchor is not a real context entry",
                    ));
                }
                return Ok(requested.to_owned());
            }
            return Err(PiError::invalid(
                "retained-tail anchor must be a real active-path entry after the checkpoint",
            ));
        }
        if path.iter().any(|entry| entry.id == requested) {
            return Ok(requested.to_owned());
        }
        Err(PiError::invalid(
            "compaction firstKeptEntryId is not a real entry on the active path",
        ))
    }

    pub(crate) fn new_entry_id(&self, subject: &str) -> Result<String, PiError> {
        fsops::new_pi_entry_id(&self.entry_ids)
            .map_err(|error| error.context(format!("generate {subject} entry id")))
    }

    /// Encodes, size-checks, durably writes and records one entry.
    pub(crate) fn append_entry(&mut self, entry: PiEntry, entry_id: String) -> Result<(), PiError> {
        let encoded = encode_pi_record(PiRecord::Entry(&entry))?;
        let record_bytes = self.reserve(&encoded)?;
        self.write_record(&encoded)?;
        self.entries.push(entry);
        self.entry_ids.insert(entry_id.clone());
        self.leaf_id = Some(entry_id);
        self.file_bytes += record_bytes;
        Ok(())
    }

    /// Rejects a record that would push the file past its cap.
    pub(crate) fn reserve(&self, encoded: &[u8]) -> Result<i64, PiError> {
        let record_bytes = encoded.len() as i64 + 1;
        if record_bytes > MAX_SESSION_FILE_BYTES as i64 - self.file_bytes {
            return Err(PiError::size(
                PiErrorKind::FileTooLarge,
                MAX_SESSION_FILE_BYTES,
            ));
        }
        Ok(record_bytes)
    }

    /// Checks the lease backing this store, if any, poisoning the store with
    /// the check's text when the lease has been lost or is not renewing.
    /// Called before every durable write.
    pub(crate) fn check_lease(&mut self) -> Result<(), PiError> {
        let check = self
            .writer_ownership
            .as_ref()
            .map(|ownership| ownership.check())
            .or_else(|| self.lease.as_ref().map(|lease| lease.check()));
        if let Some(Err(text)) = check {
            let fatal = PiError::fatal(text);
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        }
        Ok(())
    }

    /// Writes one record and `fsync`s it, poisoning the store on failure.
    pub(crate) fn write_record(&mut self, encoded: &[u8]) -> Result<(), PiError> {
        self.check_lease()?;
        let Some(file) = self.file.as_mut() else {
            let fatal = PiError::fatal("write session record: session file is not open");
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        };
        if let Err(error) = write_pi_record(file.as_mut(), encoded) {
            let fatal = PiError::fatal(error);
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        }
        Ok(())
    }
}
