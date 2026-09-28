//! The native, file-backed session store.
//!
//! One [`Store`] owns one Pi v3 JSONL file: line 1 is the session header, every
//! later line is one entry.
//!
//! Ownership: a `Store` owns its file descriptor and closes it in
//! [`Store::close`] or on drop. The descriptor holds an exclusive advisory
//! lock, so another Otto process cannot open the same session to mutate it.
//! Concurrency: all mutable state sits behind one mutex, so a `Store` is
//! `Send + Sync` and every method may be called from any thread. Errors: every
//! failure is a [`PiError`]; a failed durable write
//! poisons the store with [`PiErrorKind::FatalPersistence`] and every later
//! write returns that same error.
//!
//! These methods are synchronous and take no cancellation token; there is no
//! cancellation plumbing here yet.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use otto_core::model::{
    Block, EffectCertainty, Message, OperationDisposition, OperationOutcome, OperationStopReason,
    Role, ToolResultMetadata, Usage,
};
use otto_core::session::compaction::{
    compaction_details_present, compaction_usage_to_pi, is_real_compaction_context_entry,
    latest_compaction_metadata, validate_compaction_checkpoint,
};
use otto_core::session::context::{
    add_resolved_usage, format_persisted_timestamp, format_rfc3339_nano, missing_tool_results,
    model_message_to_pi_entry, parse_rfc3339, pending_tool_calls, snapshot_from_state,
};
use otto_core::session::pi::{
    PiCompaction, PiCustom, PiEntry, PiFile, PiSessionInfo, PiThinkingLevelChange,
};
use otto_core::session::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionMetadata, DecodedOperationFact, Header,
    MAX_SESSION_ENTRY_BYTES, MAX_SESSION_FILE_BYTES, OPERATION_CUSTOM_TYPE,
    OTTO_RUNTIME_CUSTOM_TYPE, OperationFact, OperationLedger, PiError, PiErrorKind, PiRecord,
    RuntimeMetadata, Session, SessionError, Snapshot, Warning, active_context_path, build_context,
    decode_operation_fact, decode_pi_file, encode_operation_fact, encode_pi_record,
    index_context_entries,
};

use crate::failover::lease;

use super::fsops;

const OPERATION_RESULT_UNAVAILABLE_TEXT: &str = "tool result unavailable after prior session interruption; the durable operation outcome is recorded in session history";

/// One tool call left unanswered when a session was taken over mid-turn:
/// the name and arguments the model called with, and whether it is the one
/// call that may have already reached the tool before the interruption.
/// Shared between the takeover repair in [`Store::from_file`] and
/// `subagent::interrupted`'s scan of an interrupted child transcript, since
/// both report the same kind of unanswered call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredCall {
    pub name: String,
    pub arguments: String,
    pub may_have_run: bool,
}

/// Builds the unanswered-call list for a message history that ends mid-turn:
/// the first pending call is marked `may_have_run`, because Otto cannot tell
/// whether it reached the tool before the interruption; the rest are marked
/// not executed, since a provider that stops mid-turn issues its tool calls
/// in order and waits for each result before continuing.
pub(crate) fn unanswered_calls_from(pending: &[Block]) -> Vec<UnansweredCall> {
    pending
        .iter()
        .enumerate()
        .map(|(index, call)| UnansweredCall {
            name: call.tool_name.clone(),
            arguments: call
                .arguments
                .as_ref()
                .map_or_else(|| "{}".to_string(), |raw| raw.get().to_string()),
            may_have_run: index == 0,
        })
        .collect()
}

/// Recorded when [`Prepared::activate`](super::prepared::Prepared::activate)
/// builds a store from a taken-over lease epoch: the holder the epoch was
/// taken from, and the calls the store's dangling-tool-call repair gave
/// synthetic results to, in the order they were repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Takeover {
    pub holder: lease::Holder,
    pub repaired: Vec<UnansweredCall>,
}

/// Everything the store mutates, behind one lock.
#[derive(Debug)]
pub(crate) struct StoreState {
    pub(crate) header: Header,
    pub(crate) root: PathBuf,
    pub(crate) messages: Vec<Message>,
    pub(crate) operation_ledger: OperationLedger,
    pub(crate) aggregate_usage: Usage,
    pub(crate) usage_present: bool,
    pub(crate) latest_compaction: Option<CompactionMetadata>,
    pub(crate) thinking_level: String,
    pub(crate) session_name: Option<String>,
    pub(crate) entries: Vec<PiEntry>,
    pub(crate) entry_ids: HashSet<String>,
    pub(crate) leaf_id: Option<String>,
    pub(crate) path: String,
    pub(crate) file: Option<File>,
    pub(crate) file_bytes: i64,
    pub(crate) fatal: Option<PiError>,
    pub(crate) closed: bool,
    /// The file [`Store::ensure_file`] creates in place of
    /// `<root>/<workspace key>/<id>.jsonl`; set for a sub-agent transcript.
    pub(crate) file_path: Option<PathBuf>,
    /// The Pi header's `parentSession`: the parent session file path.
    pub(crate) parent_session: Option<String>,
    /// The lease backing this session, when it is lease-managed. Checked
    /// before every durable write; see [`StoreState::check_lease`].
    pub(crate) lease: Option<Arc<lease::Lease>>,
    /// Whether `close` releases `lease`: true for the store that acquired
    /// it, false for a child transcript that only checks the parent's
    /// lease (see [`Store::set_lease_check`]).
    pub(crate) owns_lease: bool,
    /// Set by `create` when the session should acquire its own lease
    /// directory on the first durable write; consumed and cleared by
    /// [`StoreState::ensure_file`].
    pub(crate) enable_failover: Option<u64>,
    /// Recorded once, by `Prepared::activate`, when this store was opened by
    /// taking over another holder's lease epoch.
    pub(crate) takeover: Option<Takeover>,
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
                operation_ledger: OperationLedger::default(),
                aggregate_usage: Usage::default(),
                usage_present: false,
                latest_compaction: None,
                thinking_level: String::new(),
                session_name: None,
                entries: Vec::new(),
                entry_ids: HashSet::new(),
                leaf_id: None,
                path: String::new(),
                file: None,
                file_bytes: 0,
                fatal: None,
                closed: false,
                file_path: None,
                parent_session: None,
                lease: None,
                owns_lease: false,
                enable_failover: None,
                takeover: None,
                #[cfg(test)]
                fail_writes: false,
            }),
        })
    }

    /// Marks this lazy, top-level store to acquire its own lease directory
    /// (`lease_seconds` as `L`) on its first durable write, in place of
    /// opening one already prepared by [`Prepared::prepare`]. For sessions
    /// `Store::create`/`create_lazy` builds directly, such as a freshly
    /// started REPL session; a child transcript never calls this and never
    /// gets its own lease directory.
    pub fn enable_failover(&self, lease_seconds: u64) -> Result<(), PiError> {
        let mut state = self.lock()?;
        state.enable_failover = Some(lease_seconds);
        Ok(())
    }

    /// Returns a lazy store for a sub-agent transcript of the parent session
    /// file `parent`: `<parent without .jsonl>/<name>.jsonl`, with the Pi
    /// header's `parentSession` set to `parent`. The directory is created
    /// `0700` on the first write, as the default layout's is.
    pub fn create_child_lazy(
        parent: impl AsRef<Path>,
        name: &str,
        header: Header,
    ) -> Result<Self, PiError> {
        let parent = parent.as_ref();
        let store = Self::create_lazy(PathBuf::new(), header)?;
        {
            let mut state = store.lock()?;
            state.file_path = Some(Self::child_path(parent, name));
            state.parent_session = Some(parent.to_string_lossy().into_owned());
        }
        Ok(store)
    }

    /// The file [`Store::create_child_lazy`] writes for `parent` and `name`.
    pub fn child_path(parent: impl AsRef<Path>, name: &str) -> PathBuf {
        parent
            .as_ref()
            .with_extension("")
            .join(format!("{name}.jsonl"))
    }

    /// Reads a session header without opening a store.
    pub fn read_header(path: impl AsRef<Path>) -> Result<Header, PiError> {
        let mut file = File::open(path.as_ref())
            .map_err(|error| PiError::other(format!("open session file: {error}")))?;
        reject_oversized_session_file(&file)?;
        let decoded = decode_pi_file_read_only(&mut file)?;
        Ok(resolve_pi_store_state(&decoded)?.header)
    }

    /// Reads a session file's full message transcript without opening it for
    /// writing: no incomplete-line or dangling-tool-call repair, so it never
    /// touches disk. For a transcript this process does not own, where
    /// [`Store::open`]'s repair-and-append behavior would be wrong.
    pub fn read_transcript(path: impl AsRef<Path>) -> Result<Vec<Message>, PiError> {
        Ok(Self::read_entries(path)?.1)
    }

    /// Reads a session file's raw entries in file order and its resolved
    /// message transcript, with the same read-only behavior as
    /// [`Store::read_transcript`]. For callers that need custom entries.
    pub fn read_entries(path: impl AsRef<Path>) -> Result<(Vec<PiEntry>, Vec<Message>), PiError> {
        let mut file = File::open(path.as_ref())
            .map_err(|error| PiError::other(format!("open session file: {error}")))?;
        reject_oversized_session_file(&file)?;
        let decoded = decode_pi_file_read_only(&mut file)?;
        let messages = resolve_pi_store_state(&decoded)?.messages;
        Ok((decoded.entries, messages))
    }

    /// Opens an existing session for appending, repairing an incomplete final
    /// line and any tool call left without a result.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Vec<Warning>), PiError> {
        let path = path.as_ref();
        let prepared = super::prepared::Prepared::prepare(path, None)?;
        prepared.activate()
    }

    /// Builds a store around an already-verified descriptor. The descriptor is
    /// consumed either way: on failure it is dropped and closed. `lease` is
    /// the lease [`Prepared::activate`](super::prepared::Prepared::activate)
    /// acquired opening `path`, when the session is lease-managed, together
    /// with whether the acquisition took over another holder's epoch; the
    /// resulting store owns that lease (releases it on
    /// [`Store::close`]/drop) and records a [`Takeover`] when it does.
    pub(crate) fn from_file(
        mut file: File,
        path: &str,
        lease: Option<(Arc<lease::Lease>, lease::Acquired)>,
    ) -> Result<(Self, Vec<Warning>), PiError> {
        fsops::lock_session_exclusive(&file)?;
        reject_oversized_session_file(&file)?;
        let (decoded, mut warnings) = decode_pi_file_for_open(&mut file, path)?;
        let state = resolve_pi_store_state(&decoded)?;
        warnings.extend(state.warnings);
        let position = file
            .seek(SeekFrom::End(0))
            .map_err(|error| PiError::other(format!("seek session file: {error}")))?;

        let (lease_value, acquired) = match lease {
            Some((lease, acquired)) => (Some(lease), Some(acquired)),
            None => (None, None),
        };
        let owns_lease = lease_value.is_some();

        let store = Self {
            state: Mutex::new(StoreState {
                header: state.header,
                root: PathBuf::new(),
                messages: state.messages,
                operation_ledger: state.operation_ledger,
                aggregate_usage: state.aggregate_usage,
                usage_present: state.usage_present,
                latest_compaction: state.latest_compaction,
                thinking_level: state.thinking_level,
                session_name: state.session_name,
                entries: decoded.entries.clone(),
                entry_ids: state.entry_ids,
                leaf_id: state.leaf_id,
                path: path.to_owned(),
                file: Some(file),
                file_bytes: position as i64,
                fatal: None,
                closed: false,
                file_path: None,
                parent_session: None,
                lease: lease_value,
                owns_lease,
                enable_failover: None,
                takeover: None,
                #[cfg(test)]
                fail_writes: false,
            }),
        };
        let (repair_warnings, repaired) = store.repair_dangling_tool_calls()?;
        warnings.extend(repair_warnings);
        if let Some(lease::Acquired::TakenOver(holder)) = acquired {
            store.lock()?.takeover = Some(Takeover { holder, repaired });
        }
        let mut guard = store.lock()?;
        if let Some(file) = guard.file.as_mut() {
            file.seek(SeekFrom::End(0)).map_err(|error| {
                PiError::other(format!("seek session file after repair: {error}"))
            })?;
        }
        drop(guard);
        Ok((store, warnings))
    }

    /// The lease backing this store, when it is lease-managed.
    pub fn lease(&self) -> Option<Arc<lease::Lease>> {
        self.lock().expect("session mutex").lease.clone()
    }

    /// Takes the takeover record left by [`Store::from_file`] when this
    /// store was opened by taking over another holder's lease epoch.
    /// `None` on every call after the first.
    pub fn take_takeover(&self) -> Option<Takeover> {
        self.lock().expect("session mutex").takeover.take()
    }

    /// Sets `lease` for check-only use: every durable write is checked
    /// against it (see [`StoreState::check_lease`]), but [`Store::close`]
    /// does not release it. For a child transcript whose lease is the
    /// parent session's.
    pub fn set_lease_check(&self, lease: Arc<lease::Lease>) {
        let mut state = self.lock().expect("session mutex");
        state.lease = Some(lease);
        state.owns_lease = false;
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

    /// Archives this store's locked session file without releasing ownership.
    pub fn archive(
        &self,
        root: &Path,
        workspace: &str,
    ) -> Result<super::prepared::ArchiveResult, PiError> {
        let mut state = self.lock()?;
        state.writable()?;
        let path = state.path.clone();
        let file = state
            .file
            .as_mut()
            .ok_or_else(|| PiError::other("session file is not open"))?;
        let metadata = file
            .metadata()
            .map_err(|error| PiError::other(format!("stat session file: {error}")))?;
        super::prepared::archive_open_file(root, workspace, Path::new(&path), file, &metadata)
    }

    /// The compaction currently in force on the active path.
    pub fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.lock()
            .expect("session mutex")
            .latest_compaction
            .clone()
    }

    /// The model thinking effort recorded on this session, empty for the
    /// provider default.
    pub fn thinking_level(&self) -> String {
        self.lock().expect("session mutex").thinking_level.clone()
    }

    /// Records the thinking effort for this session. A no-op when unchanged;
    /// the file is created on the first real change so the level survives
    /// resuming the session without requiring a profile-level default.
    pub fn update_thinking_level(&self, thinking: &str) -> Result<(), PiError> {
        let mut state = self.lock()?;
        state.writable()?;
        let thinking = normalize_session_thinking(thinking)?;
        if state.thinking_level == thinking {
            return Ok(());
        }
        state.ensure_file_fatal()?;

        let timestamp = format_persisted_timestamp(Utc::now(), "thinking level update")?;
        let entry_id = state.new_entry_id("thinking level")?;
        let mut entry = PiEntry::new(
            "thinking_level_change",
            &entry_id,
            state.leaf_id.clone(),
            &timestamp,
        );
        entry.thinking_level_change = Some(PiThinkingLevelChange {
            thinking_level: thinking_to_pi_level(&thinking),
        });
        state.append_entry(entry, entry_id)?;
        state.thinking_level = thinking;
        Ok(())
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

    /// A snapshot of the operation facts folded along the active path.
    pub fn operation_ledger(&self) -> OperationLedger {
        self.lock().expect("session mutex").operation_ledger.clone()
    }

    /// Appends a validated operation fact and commits its in-memory ledger
    /// state only after the custom entry has been fsynced.
    pub fn append_operation_fact(&self, fact: OperationFact) -> Result<(), PiError> {
        let mut state = self.lock()?;
        state.writable()?;
        let mut candidate = state.operation_ledger.clone();
        candidate
            .apply(fact.clone())
            .map_err(|error| PiError::invalid(error.to_string()))?;
        let encoded =
            encode_operation_fact(&fact).map_err(|error| PiError::invalid(error.to_string()))?;
        state.append_custom_entry_unchecked(OPERATION_CUSTOM_TYPE, &encoded)?;
        state.operation_ledger = candidate;
        Ok(())
    }

    /// Appends a `custom` entry with the given `customType` and pre-encoded
    /// JSON `data`, unconditionally (unlike [`Store::update_runtime`], which
    /// skips the write when nothing changed). The
    /// [`Session::append_custom`](otto_core::session::Session::append_custom)
    /// override for `Store` calls this.
    pub fn append_custom_entry(&self, custom_type: &str, data: &str) -> Result<(), PiError> {
        if custom_type == OPERATION_CUSTOM_TYPE {
            return match decode_operation_fact(data)
                .map_err(|error| PiError::invalid(error.to_string()))?
            {
                DecodedOperationFact::Fact(fact) => self.append_operation_fact(fact),
                DecodedOperationFact::Unsupported { .. } => {
                    let mut state = self.lock()?;
                    state.writable()?;
                    state.append_custom_entry_unchecked(custom_type, data)
                }
            };
        }
        let mut state = self.lock()?;
        state.writable()?;
        state.append_custom_entry_unchecked(custom_type, data)
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
        state.operation_ledger = resolved.operation_ledger;
        state.aggregate_usage = resolved.usage;
        state.usage_present = resolved.usage_present;
        state.file_bytes += record_bytes;
        state.latest_compaction = Some(metadata.clone());
        Ok(metadata)
    }

    /// Closes the session file. Idempotent. Releases the lease this store
    /// acquired (see [`Store::set_lease_check`] for a store that only
    /// checks a lease it does not own), but only once the file has synced,
    /// so a lease is never given up while a write to it might still be in
    /// flight on disk.
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
            .map_err(|error| PiError::other(format!("close session file: {error}")))?;
        if state.owns_lease
            && let Some(lease) = state.lease.as_ref()
        {
            lease
                .release()
                .map_err(|error| PiError::other(format!("release session lease: {error}")))?;
        }
        Ok(())
    }

    /// Repairs trailing tool calls according to their durable operation facts.
    /// A terminal fact is always persisted before its synthetic result.
    fn repair_dangling_tool_calls(&self) -> Result<(Vec<Warning>, Vec<UnansweredCall>), PiError> {
        let pending = pending_tool_calls(&self.messages())?;
        let repaired = unanswered_calls_from(&pending);
        let legacy_stand_ins = missing_tool_results(&pending);
        let mut warnings = Vec::new();
        for (index, call) in pending.into_iter().enumerate() {
            let record = self
                .lock()?
                .operation_ledger
                .operation_for_tool_call(&call.tool_call_id)
                .cloned();
            let block = match record {
                Some(record) => {
                    if record.corrupt {
                        warnings.push(Warning::new(format!(
                            "operation history for tool call {} is corrupt; effects are unknown",
                            call.tool_call_id
                        )));
                    }
                    let operation_id = record.operation_id.clone();
                    let outcome = if record.corrupt {
                        OperationOutcome {
                            disposition: OperationDisposition::Interrupted,
                            effect_certainty: EffectCertainty::Unknown,
                            stop_reason: Some(OperationStopReason::ProcessLost),
                        }
                    } else {
                        record.terminal.clone().unwrap_or(OperationOutcome {
                            disposition: OperationDisposition::Interrupted,
                            effect_certainty: EffectCertainty::Unknown,
                            stop_reason: Some(OperationStopReason::ProcessLost),
                        })
                    };
                    if record.terminal.is_none() {
                        let terminal = OperationFact::terminal(
                            operation_id.clone(),
                            record.attempts,
                            record.tool_call_id,
                            record.tool_name,
                            outcome.clone(),
                        );
                        if let Err(error) = self.append_operation_fact(terminal) {
                            if !record.corrupt {
                                return Err(error.context(format!(
                                    "repair operation for dangling tool call {:?}",
                                    call.tool_call_id
                                )));
                            }
                            warnings.push(Warning::new(format!(
                                "could not settle corrupt operation for tool call {}; effects are unknown",
                                call.tool_call_id
                            )));
                        }
                    }
                    Block {
                        block_type: otto_core::model::BlockType::ToolResult,
                        text: OPERATION_RESULT_UNAVAILABLE_TEXT.into(),
                        tool_call_id: call.tool_call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        is_error: true,
                        operation_metadata: Some(ToolResultMetadata {
                            operation_id: Some(operation_id),
                            disposition: outcome.disposition,
                            effect_certainty: outcome.effect_certainty,
                            stop_reason: outcome.stop_reason,
                        }),
                        ..Block::default()
                    }
                }
                None => legacy_stand_ins[index].clone(),
            };
            let message = Message {
                role: Role::Tool,
                created_at: Utc::now(),
                blocks: vec![block],
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
        Ok((warnings, repaired))
    }
}

impl Drop for Store {
    /// Best-effort: releases a lease this store owns and never released,
    /// e.g. because the caller dropped the store without calling
    /// [`Store::close`]. Errors are not observable from `drop` and are
    /// discarded.
    fn drop(&mut self) {
        let Ok(state) = self.state.lock() else {
            return;
        };
        if !state.closed
            && state.owns_lease
            && let Some(lease) = state.lease.as_ref()
        {
            let _ = lease.release();
        }
    }
}

impl StoreState {
    fn append_custom_entry_unchecked(
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
            data: Some(raw_value(data.to_owned())?),
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

    /// Creates the directory, file, header and runtime entry. A no-op once the
    /// file exists.
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
                let directory = super::list::session_directory(&self.root, &self.header.workspace)?;
                let path = directory.join(format!("{}.jsonl", self.header.id));
                (directory, path)
            }
        };

        std::fs::create_dir_all(&directory)
            .map_err(|error| PiError::other(format!("create session directory: {error}")))?;
        std::fs::set_permissions(
            &directory,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .map_err(|error| PiError::other(format!("chmod session directory: {error}")))?;

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
            let (lease, _acquired) = lease::Lease::acquire(&path)
                .map_err(|error| PiError::other(format!("acquire session lease: {error}")))?;
            self.lease = Some(lease);
            self.owns_lease = true;
        }

        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC)
                .open(&path)
                .map_err(|error| PiError::other(format!("create session file: {error}")))?
        };

        let result = fsops::lock_session_exclusive(&file)
            .and_then(|_| self.check_lease())
            .and_then(|_| self.write_initial_records(&mut file));
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
            parent_session: self.parent_session.clone(),
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

    /// Checks the lease backing this store, if any, poisoning the store with
    /// the check's text when the lease has been lost or is not renewing.
    /// Called before every durable write.
    pub(crate) fn check_lease(&mut self) -> Result<(), PiError> {
        if let Some(lease) = self.lease.as_ref()
            && let Err(text) = lease.check()
        {
            let fatal = PiError::fatal(text);
            self.fatal = Some(fatal.clone());
            return Err(fatal);
        }
        Ok(())
    }

    /// Writes one record and `fsync`s it, poisoning the store on failure.
    pub(crate) fn write_record(&mut self, encoded: &[u8]) -> Result<(), PiError> {
        self.check_lease()?;
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

    fn append_operation_fact(&self, fact: OperationFact) -> Result<(), SessionError> {
        Store::append_operation_fact(self, fact)
            .map_err(|error| SessionError::Persist(error.to_string()))
    }

    fn operation_ledger(&self) -> OperationLedger {
        Store::operation_ledger(self)
    }

    fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError> {
        Store::append_custom_entry(self, custom_type, data)
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
    pub(crate) thinking_level: String,
    pub(crate) session_name: Option<String>,
    pub(crate) entry_ids: HashSet<String>,
    pub(crate) leaf_id: Option<String>,
    pub(crate) operation_ledger: OperationLedger,
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
    let thinking_level = pi_level_to_thinking(&resolved.thinking_level)?;
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
        thinking_level,
        session_name,
        entry_ids,
        leaf_id,
        operation_ledger: resolved.operation_ledger,
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

fn normalize_session_thinking(thinking: &str) -> Result<String, PiError> {
    match thinking {
        "" | "low" | "medium" | "high" | "xhigh" | "max" => Ok(thinking.to_string()),
        _ => Err(PiError::invalid(
            "invalid thinking: must be one of low, medium, high, xhigh, max",
        )),
    }
}

fn thinking_to_pi_level(thinking: &str) -> String {
    if thinking.is_empty() {
        "off".to_string()
    } else {
        thinking.to_string()
    }
}

fn pi_level_to_thinking(thinking: &str) -> Result<String, PiError> {
    match thinking {
        "" | "off" => Ok(String::new()),
        "low" | "medium" | "high" | "xhigh" | "max" => Ok(thinking.to_string()),
        _ => Err(PiError::invalid(
            "invalid thinking level: must be one of off, low, medium, high, xhigh, max",
        )),
    }
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
