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
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use otto_core::model::{
    Block, EffectCertainty, Message, OperationDisposition, OperationOutcome, OperationStopReason,
    Role, ToolResultMetadata, Usage,
};
use otto_core::session::InterruptedTurn;
use otto_core::session::compaction::{
    compaction_details_present, compaction_usage_to_pi, is_real_compaction_context_entry,
    latest_compaction_metadata, project_compaction_metadata, validate_compaction_checkpoint,
};
use otto_core::session::context::{
    add_resolved_usage, format_persisted_timestamp, format_rfc3339_nano, missing_tool_results,
    model_message_to_pi_entry, parse_rfc3339, pending_tool_calls, snapshot_from_state,
};
use otto_core::session::operation::replay_allowed;
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

use crate::storage::backends::posix::lease::{self, Keeper};

use crate::session::fsops;
use crate::storage::files::{FileDriver, FileRoot, JournalFile, JournalOpen};

mod lifecycle;
mod open;
mod pi_file;
mod session_impl;
mod state;

pub(crate) use pi_file::*;

const OPERATION_RESULT_UNAVAILABLE_TEXT: &str = "tool result unavailable after prior session interruption; the durable operation outcome is recorded in session history";

/// One tool call left unanswered when a session was taken over mid-turn:
/// the name and arguments the model called with, and whether it is the one
/// call that may have already reached the tool before the interruption.
/// Shared between the recovery repair in [`Store::from_file`] and
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

/// What [`Store::repair_dangling_tool_calls`] did: its warnings, the calls it
/// answered with synthetic results, and the calls it left for the agent to run
/// again. At most one of the last two is non-empty.
type RepairOutcome = (Vec<Warning>, Vec<UnansweredCall>, Vec<UnansweredCall>);

/// Why [`Store::from_file`] recovered a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryCause {
    /// [`Prepared::activate`](super::prepared::Prepared::activate) took over
    /// another holder's lease epoch.
    TakenOver(lease::Holder),
    /// The previous writer stopped without finishing its turn: the file
    /// ends with unanswered tool calls or an `otto.turn` start without its
    /// end. The exclusive file lock guarantees that writer is gone.
    ProcessLost,
}

/// Recorded when [`Store::from_file`] opens a session the previous writer
/// left mid-turn, or by taking over a lease epoch: the cause, the calls the
/// store's dangling-tool-call repair gave synthetic results to, in the order
/// they were repaired, and the interrupted turn, if one was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    pub cause: RecoveryCause,
    pub repaired: Vec<UnansweredCall>,
    /// Calls left without a result on purpose: every pending call of the
    /// trailing assistant message is replayable, so the store neither
    /// settled nor answered them and the agent runs them again before the
    /// next provider request. Empty whenever `repaired` is not.
    pub replayable: Vec<UnansweredCall>,
    /// The newest `otto.turn` start without its end, when the file has one.
    pub interrupted_turn: Option<InterruptedTurn>,
}

impl Recovery {
    /// Whether the interrupted turn already used its automatic resume.
    pub fn resume_exhausted(&self) -> bool {
        self.interrupted_turn
            .as_ref()
            .is_some_and(InterruptedTurn::resume_exhausted)
    }
}

/// Everything the store mutates, behind one lock.
#[derive(Debug)]
pub(crate) struct StoreState {
    pub(crate) header: Header,
    pub(crate) root: PathBuf,
    pub(crate) messages: Vec<Message>,
    /// Input ids delivered on the active path, kept across compaction; see
    /// [`otto_core::session::context::ResolvedContext::input_ids`].
    pub(crate) input_ids: HashSet<String>,
    pub(crate) operation_ledger: OperationLedger,
    pub(crate) aggregate_usage: Usage,
    pub(crate) usage_present: bool,
    pub(crate) latest_compaction: Option<CompactionMetadata>,
    pub(crate) thinking_level: String,
    pub(crate) session_name: Option<String>,
    /// The `otto.fork` record, read once on open: only a fork's creation
    /// writes one.
    pub(crate) forked_from: Option<otto_core::session::ForkRecord>,
    /// The plan in force: folded on open, then replaced by each appended
    /// valid `otto.plan` record.
    pub(crate) plan: Vec<otto_core::session::PlanStep>,
    pub(crate) entries: Vec<PiEntry>,
    pub(crate) entry_ids: HashSet<String>,
    pub(crate) leaf_id: Option<String>,
    pub(crate) path: String,
    pub(crate) file: Option<Box<dyn JournalFile>>,
    pub(crate) file_driver: FileDriverRef,
    pub(crate) keeper: KeeperRef,
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
    /// Neutral check/release authority consumed by runtime and child-session
    /// ports; the protocol-specific lease stays inside this POSIX adapter.
    pub(crate) writer_ownership: Option<Arc<dyn crate::session::WriterOwnership>>,
    /// Whether `close` releases `lease`: true for the store that acquired
    /// it, false for a child transcript that only checks the parent's
    /// lease (see [`Store::set_lease_check`]).
    pub(crate) owns_lease: bool,
    /// Set by `create` when the session should acquire its own lease
    /// directory on the first durable write; consumed and cleared by
    /// [`StoreState::ensure_file`].
    pub(crate) enable_failover: Option<u64>,
    /// Recorded once, by [`Store::from_file`], when this store was opened
    /// from a session its previous writer left mid-turn or by taking over
    /// another holder's lease epoch.
    pub(crate) recovery: Option<Recovery>,
}

/// An append-only session file.
#[derive(Debug)]
pub struct Store {
    pub(crate) state: Mutex<StoreState>,
}

#[derive(Clone)]
pub(crate) struct FileDriverRef(pub(crate) Arc<dyn FileDriver>);

impl std::fmt::Debug for FileDriverRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FileDriver")
    }
}

#[derive(Clone)]
pub(crate) struct KeeperRef(pub(crate) Arc<Keeper>);

impl std::fmt::Debug for KeeperRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LeaseKeeper")
    }
}

pub(super) fn open_path_journal(
    driver: &dyn FileDriver,
    path: &Path,
    mode: JournalOpen,
) -> Result<Box<dyn JournalFile>, PiError> {
    let parent = path
        .parent()
        .ok_or_else(|| PiError::invalid("session file has no parent directory"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PiError::invalid("session file name is not a valid component"))?;
    let root = driver
        .open_root(parent)
        .map_err(|error| PiError::other(format!("open session directory: {error}")))?;
    root.open_journal(name, mode)
        .map_err(|error| PiError::other(format!("open session file: {error}")))
}

fn open_or_create_root(driver: &dyn FileDriver, path: &Path) -> std::io::Result<Box<dyn FileRoot>> {
    match driver.open_root(path) {
        Ok(root) => Ok(root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    std::io::Error::other("session root name is not a valid component")
                })?;
            let parent_root = open_or_create_root(driver, parent)?;
            match parent_root.create_directory(name) {
                Ok(()) => parent_root.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            parent_root.open_directory(name)
        }
        Err(error) => Err(error),
    }
}

impl Store {
    pub(crate) fn create_lazy_with_driver_and_keeper(
        root: impl AsRef<Path>,
        mut header: Header,
        driver: Arc<dyn FileDriver>,
        keeper: Arc<Keeper>,
    ) -> Result<Self, PiError> {
        header.version = CURRENT_VERSION;
        validate_domain_header(&header)?;
        Ok(Self {
            state: Mutex::new(StoreState {
                header,
                root: root.as_ref().to_path_buf(),
                messages: Vec::new(),
                input_ids: HashSet::new(),
                operation_ledger: OperationLedger::default(),
                aggregate_usage: Usage::default(),
                usage_present: false,
                latest_compaction: None,
                thinking_level: String::new(),
                session_name: None,
                forked_from: None,
                plan: Vec::new(),
                entries: Vec::new(),
                entry_ids: HashSet::new(),
                leaf_id: None,
                path: String::new(),
                file: None,
                file_driver: FileDriverRef(driver),
                keeper: KeeperRef(keeper),
                file_bytes: 0,
                fatal: None,
                closed: false,
                file_path: None,
                parent_session: None,
                lease: None,
                writer_ownership: None,
                owns_lease: false,
                enable_failover: None,
                recovery: None,
            }),
        })
    }

    pub(crate) fn create_child_lazy_with_driver_and_keeper(
        parent: impl AsRef<Path>,
        name: &str,
        header: Header,
        driver: Arc<dyn FileDriver>,
        keeper: Arc<Keeper>,
    ) -> Result<Self, PiError> {
        let parent = parent.as_ref();
        let store =
            Self::create_lazy_with_driver_and_keeper(PathBuf::new(), header, driver, keeper)?;
        let mut state = store.lock()?;
        state.file_path = Some(Self::child_path(parent, name));
        state.parent_session = Some(parent.to_string_lossy().into_owned());
        drop(state);
        Ok(store)
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

    /// The lease backing this store, when it is lease-managed.
    pub fn lease(&self) -> Option<Arc<dyn crate::session::WriterOwnership>> {
        self.writer_ownership()
    }

    pub fn writer_ownership(&self) -> Option<Arc<dyn crate::session::WriterOwnership>> {
        let state = self.lock().expect("session mutex");
        state.writer_ownership.clone().or_else(|| {
            state
                .lease
                .as_ref()
                .map(|lease| Arc::clone(lease) as Arc<dyn crate::session::WriterOwnership>)
        })
    }

    /// Takes the recovery record left by [`Store::from_file`] when this
    /// store was opened from a session left mid-turn or by taking over
    /// another holder's lease epoch. `None` on every call after the first.
    /// The `otto.fork` record naming where this session was forked from.
    pub fn forked_from(&self) -> Option<otto_core::session::ForkRecord> {
        self.lock().ok()?.forked_from.clone()
    }

    pub fn take_recovery(&self) -> Option<Recovery> {
        self.lock().expect("session mutex").recovery.take()
    }

    /// Sets `lease` for check-only use: every durable write is checked
    /// against it (see [`StoreState::check_lease`]), but [`Store::close`]
    /// does not release it. For a child transcript whose lease is the
    /// parent session's.
    pub fn set_lease_check(&self, ownership: Arc<dyn crate::session::WriterOwnership>) {
        let mut state = self.lock().expect("session mutex");
        state.writer_ownership = Some(ownership);
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
        super::prepared::archive_open_file(
            root,
            workspace,
            Path::new(&path),
            file.as_mut(),
            &metadata,
        )
    }

    /// The compaction currently in force on the active path.
    pub fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.lock()
            .expect("session mutex")
            .latest_compaction
            .clone()
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
        let runtime_json = serde_json::to_string(runtime)
            .map_err(|error| PiError::other(format!("encode runtime metadata: {error}")))?;
        let mut entry = PiEntry::new("custom", &entry_id, state.leaf_id.clone(), &timestamp);
        entry.custom = Some(PiCustom {
            custom_type: OTTO_RUNTIME_CUSTOM_TYPE.to_owned(),
            data: Some(raw_value(runtime_json)?),
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
        let plan = if custom_type == otto_core::session::PLAN_CUSTOM_TYPE {
            Some(
                otto_core::session::decode_plan(data)
                    .ok_or_else(|| PiError::invalid("malformed otto.plan data"))?,
            )
        } else {
            None
        };
        let mut state = self.lock()?;
        state.writable()?;
        state.append_custom_entry_unchecked(custom_type, data)?;
        if let Some(plan) = plan {
            state.plan = plan;
        }
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
        state.append_message(message)
    }

    /// Appends `message` with input id `input_id` unless a message with that
    /// id is already on the active path; see
    /// [`Session::append_input_if_absent`]. Returns whether it appended.
    pub fn append_input_if_absent(
        &self,
        input_id: &str,
        message: &Message,
    ) -> Result<bool, PiError> {
        let mut state = self.lock()?;
        if state.input_ids.contains(input_id) {
            return Ok(false);
        }
        let mut message = message.clone();
        message.input_id = input_id.to_owned();
        state.append_message(&message)?;
        Ok(true)
    }

    /// Whether a message with input id `input_id` is on the active path.
    pub fn has_input(&self, input_id: &str) -> bool {
        self.lock()
            .map(|state| state.input_ids.contains(input_id))
            .unwrap_or(false)
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
        if !state.closed && state.owns_lease {
            if let Some(ownership) = state.writer_ownership.as_ref() {
                let _ = ownership.release();
            } else if let Some(lease) = state.lease.as_ref() {
                let _ = lease.release();
            }
        }
    }
}
