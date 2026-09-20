//! The native, file-backed session store.
//!
//! One [`Store`] owns one Pi v3 JSONL file: line 1 is the session header, every
//! later line is one entry.
//!
//! Ownership: a `Store` owns its file descriptor and closes it in
//! [`Store::close`] or on drop. Concurrency: all mutable state sits behind one
//! mutex, so a `Store` is `Send + Sync` and every method may be called from any
//! thread. Errors: every failure is a [`PiError`]; a failed durable write
//! poisons the store with [`PiErrorKind::FatalPersistence`] and every later
//! write returns that same error.
//!
//! These methods are synchronous and take no cancellation token; there is no
//! cancellation plumbing here yet.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use otto_core::model::{Message, Role, Usage};
use otto_core::session::compaction::{
    compaction_details_present, compaction_usage_to_pi, is_real_compaction_context_entry,
    latest_compaction_metadata, validate_compaction_checkpoint,
};
use otto_core::session::context::{
    add_resolved_usage, format_persisted_timestamp, format_rfc3339_nano, model_message_to_pi_entry,
    parse_rfc3339, pending_tool_calls, snapshot_from_state,
};
use otto_core::session::pi::{PiCompaction, PiCustom, PiEntry, PiFile, PiSessionInfo};
use otto_core::session::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionMetadata, Header, MAX_SESSION_ENTRY_BYTES,
    MAX_SESSION_FILE_BYTES, OTTO_RUNTIME_CUSTOM_TYPE, PiError, PiErrorKind, PiRecord,
    RuntimeMetadata, Session, SessionError, Snapshot, Warning, active_context_path, build_context,
    decode_pi_file, encode_pi_record, index_context_entries,
};

use super::fsops;

/// Everything the store mutates, behind one lock.
#[derive(Debug)]
pub(crate) struct StoreState {
    pub(crate) header: Header,
    pub(crate) root: PathBuf,
    pub(crate) messages: Vec<Message>,
    pub(crate) aggregate_usage: Usage,
    pub(crate) usage_present: bool,
    pub(crate) latest_compaction: Option<CompactionMetadata>,
    pub(crate) session_name: Option<String>,
    pub(crate) entries: Vec<PiEntry>,
    pub(crate) entry_ids: HashSet<String>,
    pub(crate) leaf_id: Option<String>,
    pub(crate) path: String,
    pub(crate) file: Option<File>,
    pub(crate) file_bytes: i64,
    pub(crate) fatal: Option<PiError>,
    pub(crate) closed: bool,
    /// Test-only fault injection for the durable-write path.
    #[cfg(test)]
    pub(crate) fail_writes: bool,
}

/// An append-only session file.
#[derive(Debug)]
pub struct Store {
    pub(crate) state: Mutex<StoreState>,
}

impl Store {
    /// Creates the session directory and file eagerly and writes the header
    /// and the initial `otto.runtime` entry.
    pub fn create(root: impl AsRef<Path>, header: Header) -> Result<Self, PiError> {
        let store = Self::create_lazy(root, header)?;
        store.lock()?.ensure_file()?;
        Ok(store)
    }

    /// Returns a store that does not touch disk until its first durable write.
    /// A session created and closed without a message leaves no file behind.
    pub fn create_lazy(root: impl AsRef<Path>, mut header: Header) -> Result<Self, PiError> {
        header.version = CURRENT_VERSION;
        validate_domain_header(&header)?;
        Ok(Self {
            state: Mutex::new(StoreState {
                header,
                root: root.as_ref().to_path_buf(),
                messages: Vec::new(),
                aggregate_usage: Usage::default(),
                usage_present: false,
                latest_compaction: None,
                session_name: None,
                entries: Vec::new(),
                entry_ids: HashSet::new(),
                leaf_id: None,
                path: String::new(),
                file: None,
                file_bytes: 0,
                fatal: None,
                closed: false,
                #[cfg(test)]
                fail_writes: false,
            }),
        })
    }

    /// Reads a session header without opening a store.
    pub fn read_header(path: impl AsRef<Path>) -> Result<Header, PiError> {
        let mut file = File::open(path.as_ref())
            .map_err(|error| PiError::other(format!("open session file: {error}")))?;
        reject_oversized_session_file(&file)?;
        let decoded = decode_pi_file_read_only(&mut file)?;
        Ok(resolve_pi_store_state(&decoded)?.header)
    }

    /// Opens an existing session for appending, repairing an incomplete final
    /// line and any tool call left without a result.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Vec<Warning>), PiError> {
        let path = path.as_ref();
        let prepared = super::prepared::Prepared::prepare(path)?;
        prepared.activate()
    }

    /// Builds a store around an already-verified descriptor. The descriptor is
    /// consumed either way: on failure it is dropped and closed.
    pub(crate) fn from_file(mut file: File, path: &str) -> Result<(Self, Vec<Warning>), PiError> {
        reject_oversized_session_file(&file)?;
        let (decoded, mut warnings) = decode_pi_file_for_open(&mut file, path)?;
        let state = resolve_pi_store_state(&decoded)?;
        warnings.extend(state.warnings);
        let position = file
            .seek(SeekFrom::End(0))
            .map_err(|error| PiError::other(format!("seek session file: {error}")))?;

        let store = Self {
            state: Mutex::new(StoreState {
                header: state.header,
                root: PathBuf::new(),
                messages: state.messages,
                aggregate_usage: state.aggregate_usage,
                usage_present: state.usage_present,
                latest_compaction: state.latest_compaction,
                session_name: state.session_name,
                entries: decoded.entries.clone(),
                entry_ids: state.entry_ids,
                leaf_id: state.leaf_id,
                path: path.to_owned(),
                file: Some(file),
                file_bytes: position as i64,
                fatal: None,
                closed: false,
                #[cfg(test)]
                fail_writes: false,
            }),
        };
        warnings.extend(store.repair_dangling_tool_calls()?);
        let mut guard = store.lock()?;
        if let Some(file) = guard.file.as_mut() {
            file.seek(SeekFrom::End(0)).map_err(|error| {
                PiError::other(format!("seek session file after repair: {error}"))
            })?;
        }
        drop(guard);
        Ok((store, warnings))
    }

    pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, StoreState>, PiError> {
        self.state
            .lock()
            .map_err(|_| PiError::other("session mutex is poisoned"))
    }

    /// The provider, model, profile and identity in force right now.
    pub fn header(&self) -> Header {
        self.lock().expect("session mutex").header.clone()
    }

    /// A copy of the active-path messages in append order.
    pub fn messages(&self) -> Vec<Message> {
        self.lock().expect("session mutex").messages.clone()
    }

    /// The summed assistant usage, and whether any was recorded.
    pub fn aggregate_usage(&self) -> (Usage, bool) {
        let state = self.lock().expect("session mutex");
        (state.aggregate_usage, state.usage_present)
    }

    /// Token accounting a frontend can render without replaying the file.
    pub fn snapshot(&self) -> Snapshot {
        let state = self.lock().expect("session mutex");
        snapshot_from_state(
            state.aggregate_usage,
            state.usage_present,
            &state.messages,
            state.latest_compaction.as_ref(),
        )
    }

    /// The session's display name, empty when it was never renamed.
    pub fn name(&self) -> String {
        self.lock()
            .expect("session mutex")
            .session_name
            .clone()
            .unwrap_or_default()
    }

    /// The session file path, empty while a lazy store has not created it.
    pub fn path(&self) -> String {
        self.lock().expect("session mutex").path.clone()
    }

    /// The compaction currently in force on the active path.
    pub fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.lock()
            .expect("session mutex")
            .latest_compaction
            .clone()
    }

    /// Records a provider, model or profile change. A no-op when nothing
    /// changed; the file is created on the first real change.
    pub fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), PiError> {
        let mut state = self.lock()?;
        state.writable()?;
        if runtime.provider.trim().is_empty() || runtime.model.trim().is_empty() {
            return Err(PiError::invalid("runtime provider and model are required"));
        }
        if state.header.profile == runtime.profile
            && state.header.provider == runtime.provider
            && state.header.model == runtime.model
        {
            return Ok(());
        }
        state.ensure_file_fatal()?;

        let timestamp = format_persisted_timestamp(Utc::now(), "runtime update")?;
        let entry_id = state.new_entry_id("runtime")?;
        let data = serde_json::to_string(runtime)
            .map_err(|error| PiError::other(format!("encode runtime metadata: {error}")))?;
        let mut entry = PiEntry::new("custom", &entry_id, state.leaf_id.clone(), &timestamp);
        entry.custom = Some(PiCustom {
            custom_type: OTTO_RUNTIME_CUSTOM_TYPE.to_owned(),
            data: Some(raw_value(data)?),
        });
        state.append_entry(entry, entry_id)?;
        state.header.profile = runtime.profile.clone();
        state.header.provider = runtime.provider.clone();
        state.header.model = runtime.model.clone();
        Ok(())
    }

    /// Records a new display name for the session.
    pub fn rename(&self, name: &str) -> Result<(), PiError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(PiError::invalid("session name is required"));
        }
        let mut state = self.lock()?;
        state.writable()?;
        state.ensure_file_fatal()?;

        let timestamp = format_persisted_timestamp(Utc::now(), "session rename")?;
        let entry_id = state.new_entry_id("session rename")?;
        let mut entry = PiEntry::new("session_info", &entry_id, state.leaf_id.clone(), &timestamp);
        entry.session_info = Some(PiSessionInfo {
            name: Some(name.to_owned()),
        });
        state.append_entry(entry, entry_id)?;
        state.session_name = Some(name.to_owned());
        Ok(())
    }

    /// Validates and durably appends one message. Nothing is stored when the
    /// call returns an error.
    pub fn append_message(&self, message: &Message) -> Result<(), PiError> {
        let mut state = self.lock()?;
        state.writable()?;
        message
            .validate()
            .map_err(|error| PiError::invalid(error.0))?;
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
        state.messages.push(persisted);
        Ok(())
    }

    /// Records a compaction checkpoint and returns the metadata now in force.
    ///
    /// Every check runs before the write: the checkpoint is validated, its
    /// anchor is resolved against the active path, and the candidate entry is
    /// replayed. Nothing is persisted and no state changes when any of them
    /// fails.
    pub fn append_compaction(
        &self,
        checkpoint: &CompactionCheckpoint,
    ) -> Result<CompactionMetadata, PiError> {
        let mut state = self.lock()?;
        state.writable()?;
        validate_compaction_checkpoint(checkpoint)?;

        let first_kept_entry_id = state.compaction_anchor(&checkpoint.first_kept_entry_id)?;
        let timestamp = format_persisted_timestamp(checkpoint.created_at, "compaction")?;
        let entry_id = state.new_entry_id("compaction")?;
        let usage = compaction_usage_to_pi(checkpoint.usage.as_ref())?;

        let mut entry = PiEntry::new("compaction", &entry_id, state.leaf_id.clone(), &timestamp);
        entry.compaction = Some(Box::new(PiCompaction {
            summary: checkpoint.summary.clone(),
            first_kept_entry_id: Some(first_kept_entry_id),
            tokens_before: checkpoint.tokens_before,
            usage,
            ..PiCompaction::default()
        }));

        let mut candidate = state.entries.clone();
        candidate.push(entry.clone());
        let (resolved, _) = build_context(&candidate, &entry_id)?;

        if compaction_details_present(&checkpoint.details) {
            let encoded = serde_json::to_string(&checkpoint.details)
                .map_err(|error| PiError::other(format!("encode compaction details: {error}")))?;
            if let Some(compaction) = entry.compaction.as_mut() {
                compaction.details = Some(raw_value(encoded)?);
            }
        }
        let last = candidate.len() - 1;
        candidate[last] = entry.clone();

        let metadata = latest_compaction_metadata(&candidate, &entry_id)?
            .filter(|metadata| metadata.id == entry_id)
            .ok_or_else(|| PiError::invalid("candidate compaction did not become active"))?;

        let encoded = encode_pi_record(PiRecord::Entry(&entry))?;
        let record_bytes = state.reserve(&encoded)?;
        state.ensure_file_fatal()?;
        state.write_record(&encoded)?;

        state.entries = candidate;
        state.entry_ids.insert(entry_id.clone());
        state.leaf_id = Some(entry_id);
        state.messages = resolved.messages;
        state.aggregate_usage = resolved.usage;
        state.usage_present = resolved.usage_present;
        state.file_bytes += record_bytes;
        state.latest_compaction = Some(metadata.clone());
        Ok(metadata)
    }

    /// Closes the session file. Idempotent.
    pub fn close(&self) -> Result<(), PiError> {
        let mut state = self.lock()?;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        let Some(file) = state.file.take() else {
            return Ok(());
        };
        file.sync_all()
            .map_err(|error| PiError::other(format!("close session file: {error}")))
    }

    /// Appends a synthetic error result for each tool call left unresolved by
    /// a session that ended mid-turn.
    fn repair_dangling_tool_calls(&self) -> Result<Vec<Warning>, PiError> {
        use otto_core::model::{Block, BlockType};
        let pending = pending_tool_calls(&self.messages())?;
        let mut warnings = Vec::new();
        for call in pending {
            let message = Message {
                role: Role::Tool,
                created_at: Utc::now(),
                blocks: vec![Block {
                    block_type: BlockType::ToolResult,
                    text: "tool result missing from prior session".into(),
                    tool_call_id: call.tool_call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    is_error: true,
                    ..Block::default()
                }],
                ..Message::default()
            };
            self.append_message(&message).map_err(|error| {
                error.context(format!("repair dangling tool call {:?}", call.tool_call_id))
            })?;
            warnings.push(Warning::new(format!(
                "repaired dangling tool call {}",
                call.tool_call_id
            )));
        }
        Ok(warnings)
    }
}

impl StoreState {
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

    /// Creates the directory, file, header and runtime entry. A no-op once the
    /// file exists.
    pub(crate) fn ensure_file(&mut self) -> Result<(), PiError> {
        if self.file.is_some() {
            return Ok(());
        }
        let directory = super::list::session_directory(&self.root, &self.header.workspace)?;
        std::fs::create_dir_all(&directory)
            .map_err(|error| PiError::other(format!("create session directory: {error}")))?;
        std::fs::set_permissions(
            &directory,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .map_err(|error| PiError::other(format!("chmod session directory: {error}")))?;

        let path = directory.join(format!("{}.jsonl", self.header.id));
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|error| PiError::other(format!("create session file: {error}")))?
        };

        let result = self.write_initial_records(&mut file);
        let (file_bytes, entry) = match result {
            Ok(value) => value,
            Err(error) => {
                drop(file);
                if let Err(remove) = std::fs::remove_file(&path)
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
        self.leaf_id = Some(entry.id.clone());
        self.entry_ids = HashSet::from([entry.id.clone()]);
        self.entries = vec![entry];
        self.file = Some(file);
        self.file_bytes = file_bytes;
        Ok(())
    }

    fn write_initial_records(&self, file: &mut File) -> Result<(i64, PiEntry), PiError> {
        use otto_core::session::pi::{PI_SESSION_VERSION, PiHeader};
        let timestamp = format_rfc3339_nano(self.header.created_at);
        let header = PiHeader {
            type_name: "session".into(),
            version: PI_SESSION_VERSION,
            id: self.header.id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.header.workspace.clone(),
            parent_session: None,
            raw: Vec::new(),
        };
        let mut file_bytes = write_pi_record(file, &encode_pi_record(PiRecord::Header(&header))?)
            .map_err(|error| error.context("write session header"))?;

        let runtime_id = fsops::new_pi_entry_id(&HashSet::new())
            .map_err(|error| error.context("generate runtime entry id"))?;
        let data = serde_json::to_string(&RuntimeMetadata {
            profile: self.header.profile.clone(),
            provider: self.header.provider.clone(),
            model: self.header.model.clone(),
        })
        .map_err(|error| PiError::other(format!("encode runtime metadata: {error}")))?;
        let mut entry = PiEntry::new("custom", &runtime_id, None, &timestamp);
        entry.custom = Some(PiCustom {
            custom_type: OTTO_RUNTIME_CUSTOM_TYPE.to_owned(),
            data: Some(raw_value(data)?),
        });
        file_bytes += write_pi_record(file, &encode_pi_record(PiRecord::Entry(&entry))?)
            .map_err(|error| error.context("write runtime metadata"))?;
        Ok((file_bytes, entry))
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

    /// Writes one record and `fsync`s it, poisoning the store on failure.
    pub(crate) fn write_record(&mut self, encoded: &[u8]) -> Result<(), PiError> {
        #[cfg(test)]
        if self.fail_writes {
            let fatal = PiError::fatal("write session record: injected failure");
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        }
        let Some(file) = self.file.as_mut() else {
            let fatal = PiError::fatal("write session record: session file is not open");
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        };
        if let Err(error) = write_pi_record(file, encoded) {
            let fatal = PiError::fatal(error);
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        }
        Ok(())
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Session for Store {
    fn messages(&self) -> Vec<Message> {
        Store::messages(self)
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        self.append_message(&message)
            .map_err(|error| SessionError::Persist(error.to_string()))
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        Store::latest_compaction(self)
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        Store::append_compaction(self, &checkpoint)
            .map_err(|error| SessionError::Persist(error.to_string()))
    }
}

/// Writes `encoded` plus its newline delimiter and `fsync`s the file.
fn write_pi_record(file: &mut File, encoded: &[u8]) -> Result<i64, PiError> {
    let mut record = Vec::with_capacity(encoded.len() + 1);
    record.extend_from_slice(encoded);
    record.push(b'\n');
    file.write_all(&record)
        .map_err(|error| PiError::other(format!("write session record: {error}")))?;
    file.sync_all()
        .map_err(|error| PiError::other(format!("sync session file: {error}")))?;
    Ok(record.len() as i64)
}

fn raw_value(text: String) -> Result<Box<serde_json::value::RawValue>, PiError> {
    serde_json::value::RawValue::from_string(text)
        .map_err(|error| PiError::other(format!("encode JSON payload: {error}")))
}

/// The domain-level header checks that run before a session file is created.
pub(crate) fn validate_domain_header(header: &Header) -> Result<String, PiError> {
    if header.id.trim().is_empty()
        || header.id.contains('/')
        || header.id.contains('\\')
        || header.id == "."
        || header.id == ".."
    {
        return Err(PiError::invalid("session id is invalid"));
    }
    if header.workspace.trim().is_empty() {
        return Err(PiError::invalid("session workspace is required"));
    }
    if header.provider.trim().is_empty() {
        return Err(PiError::invalid("session provider is required"));
    }
    if header.model.trim().is_empty() {
        return Err(PiError::invalid("session model is required"));
    }
    format_persisted_timestamp(header.created_at, "session")
}

/// The Otto-level state a decoded file resolves to.
pub(crate) struct ResolvedStoreState {
    pub(crate) header: Header,
    pub(crate) messages: Vec<Message>,
    pub(crate) aggregate_usage: Usage,
    pub(crate) usage_present: bool,
    pub(crate) latest_compaction: Option<CompactionMetadata>,
    pub(crate) session_name: Option<String>,
    pub(crate) entry_ids: HashSet<String>,
    pub(crate) leaf_id: Option<String>,
    pub(crate) warnings: Vec<Warning>,
}

pub(crate) fn resolve_pi_store_state(decoded: &PiFile) -> Result<ResolvedStoreState, PiError> {
    let created_at = validate_pi_header(&decoded.header)?;
    let entry_ids: HashSet<String> = decoded
        .entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    let leaf_id = decoded.entries.last().map(|entry| entry.id.clone());
    let leaf = leaf_id.clone().unwrap_or_default();
    let (resolved, warnings) = build_context(&decoded.entries, &leaf)?;
    let latest_compaction = latest_compaction_metadata(&decoded.entries, &leaf)?;
    let session_name = (!resolved.session_name.is_empty()).then(|| resolved.session_name.clone());
    Ok(ResolvedStoreState {
        header: Header {
            version: CURRENT_VERSION,
            id: decoded.header.id.clone(),
            workspace: decoded.header.cwd.clone(),
            provider: resolved.runtime.provider,
            profile: resolved.runtime.profile,
            model: resolved.runtime.model,
            created_at,
        },
        messages: resolved.messages,
        aggregate_usage: resolved.usage,
        usage_present: resolved.usage_present,
        latest_compaction,
        session_name,
        entry_ids,
        leaf_id,
        warnings,
    })
}

pub(crate) fn validate_pi_header(
    header: &otto_core::session::pi::PiHeader,
) -> Result<DateTime<Utc>, PiError> {
    if header.id.trim().is_empty() {
        return Err(PiError::invalid("session header id is required"));
    }
    if header.cwd.trim().is_empty() {
        return Err(PiError::invalid("session header cwd is required"));
    }
    parse_rfc3339(&header.timestamp)
        .ok_or_else(|| PiError::invalid("session header timestamp is invalid"))
}

/// Rejects a file above [`MAX_SESSION_FILE_BYTES`] before any of it is read.
pub(crate) fn reject_oversized_session_file(file: &File) -> Result<(), PiError> {
    let metadata = file
        .metadata()
        .map_err(|error| PiError::other(format!("stat session file: {error}")))?;
    if metadata.len() > MAX_SESSION_FILE_BYTES as u64 {
        return Err(PiError::size(
            PiErrorKind::FileTooLarge,
            MAX_SESSION_FILE_BYTES,
        ));
    }
    Ok(())
}

/// Where the final record starts, whether the file lacks a trailing newline,
/// and whether that final record is truncated JSON.
fn final_pi_record_state(file: &mut File) -> Result<(u64, bool, bool), PiError> {
    let size = file
        .metadata()
        .map_err(|error| PiError::other(format!("stat session file: {error}")))?
        .len();
    if size == 0 {
        return Ok((0, false, false));
    }
    let mut last = [0u8; 1];
    read_exact_at(file, &mut last, size - 1)
        .map_err(|error| PiError::other(format!("read session tail: {error}")))?;
    if last[0] == b'\n' {
        return Ok((0, false, false));
    }
    let window = MAX_SESSION_ENTRY_BYTES as u64 + 1;
    let start = size.saturating_sub(window);
    let mut buffer = vec![0u8; (size - start) as usize];
    read_exact_at(file, &mut buffer, start)
        .map_err(|error| PiError::other(format!("read final session record: {error}")))?;
    let separator = buffer.iter().rposition(|byte| *byte == b'\n');
    let Some(separator) = separator else {
        if start > 0 {
            return Ok((0, true, false));
        }
        return Ok((start, true, is_incomplete_json(&buffer)));
    };
    let final_start = start + separator as u64 + 1;
    Ok((
        final_start,
        true,
        is_incomplete_json(&buffer[separator + 1..]),
    ))
}

fn read_exact_at(file: &mut File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)
}

/// True when the line is valid JSON that simply stops early, which is what a
/// process killed mid-append leaves behind.
fn is_incomplete_json(line: &[u8]) -> bool {
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(_) => false,
        Err(error) => error.is_eof(),
    }
}

fn read_all(file: &mut File, start: u64, end: Option<u64>) -> Result<Vec<u8>, PiError> {
    file.seek(SeekFrom::Start(start))
        .map_err(|error| PiError::other(format!("seek session file: {error}")))?;
    let mut data = Vec::new();
    match end {
        Some(end) => {
            data.resize((end - start) as usize, 0);
            file.read_exact(&mut data)
        }
        None => file.read_to_end(&mut data).map(|_| ()),
    }
    .map_err(|error| PiError::other(format!("read session file: {error}")))?;
    Ok(data)
}

/// Decodes a session file without repairing it.
pub(crate) fn decode_pi_file_read_only(file: &mut File) -> Result<PiFile, PiError> {
    let (final_start, _, incomplete) = final_pi_record_state(file)?;
    let end = (incomplete && final_start > 0).then_some(final_start);
    decode_pi_file(&read_all(file, 0, end)?)
}

/// Decodes a session file for appending, repairing a truncated final record or
/// a missing final newline. Both repairs are rejected when the repaired file
/// would not resolve to a valid session.
fn decode_pi_file_for_open(file: &mut File, path: &str) -> Result<(PiFile, Vec<Warning>), PiError> {
    let (final_start, missing_lf, incomplete) = final_pi_record_state(file)?;
    if incomplete && final_start > 0 {
        let decoded = decode_pi_file(&read_all(file, 0, Some(final_start))?)?;
        resolve_pi_store_state(&decoded)?;
        file.set_len(final_start)
            .map_err(|error| PiError::other(format!("truncate session file: {error}")))?;
        file.seek(SeekFrom::Start(final_start))
            .map_err(|error| PiError::other(format!("seek truncated session file: {error}")))?;
        file.sync_all()
            .map_err(|error| PiError::other(format!("sync truncated session file: {error}")))?;
        return Ok((
            decoded,
            vec![Warning::new(format!(
                "truncated incomplete final session line at {path}"
            ))],
        ));
    }
    let decoded = decode_pi_file(&read_all(file, 0, None)?)?;
    let mut warnings = Vec::new();
    if missing_lf {
        resolve_pi_store_state(&decoded)?;
        file.seek(SeekFrom::End(0)).map_err(|error| {
            PiError::other(format!("seek session file for delimiter repair: {error}"))
        })?;
        file.write_all(b"\n")
            .map_err(|error| PiError::other(format!("write session delimiter: {error}")))?;
        file.sync_all()
            .map_err(|error| PiError::other(format!("sync session delimiter: {error}")))?;
        warnings.push(Warning::new(format!(
            "repaired missing final session delimiter at {path}"
        )));
    }
    Ok((decoded, warnings))
}
