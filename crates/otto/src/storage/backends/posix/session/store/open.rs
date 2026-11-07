//! Read-only session file accessors and `Store::from_file`.

use super::*;

impl Store {
    /// The file [`Store::create_child_lazy`] writes for `parent` and `name`.
    pub fn child_path(parent: impl AsRef<Path>, name: &str) -> PathBuf {
        parent
            .as_ref()
            .with_extension("")
            .join(format!("{name}.jsonl"))
    }

    /// Reads a session header without opening a store.
    pub fn read_header(path: impl AsRef<Path>) -> Result<Header, PiError> {
        let mut file = open_path_journal(
            &crate::storage::drivers::posix::PosixDriver,
            path.as_ref(),
            JournalOpen::ReadOnly,
        )?;
        reject_oversized_session_file(file.as_ref())?;
        let decoded = decode_pi_file_read_only(file.as_mut())?;
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
        let mut file = open_path_journal(
            &crate::storage::drivers::posix::PosixDriver,
            path.as_ref(),
            JournalOpen::ReadOnly,
        )?;
        reject_oversized_session_file(file.as_ref())?;
        let decoded = decode_pi_file_read_only(file.as_mut())?;
        let messages = resolve_pi_store_state(&decoded)?.messages;
        Ok((decoded.entries, messages))
    }

    /// Validates a bounded read from a caller-owned pinned descriptor without
    /// reopening a potentially replaced path. Recovery uses the same store
    /// state validation as ordinary read-only transcript access.
    pub(crate) fn decode_recovery_bytes(data: &[u8]) -> Result<(PiFile, Vec<Message>), PiError> {
        let decoded = match otto_core::session::decode_pi_file(data) {
            Ok(decoded) => decoded,
            Err(error) => {
                // Match read-only Store access: only an incomplete final JSON
                // record can be excluded. Valid-but-invalid lifecycle records
                // and corruption in any earlier line remain unavailable.
                let start = data
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map(|index| index + 1)
                    .unwrap_or(0);
                let incomplete = serde_json::from_slice::<serde_json::Value>(&data[start..])
                    .err()
                    .is_some_and(|error| error.is_eof());
                if start == 0 || start == data.len() || !incomplete {
                    return Err(error);
                }
                otto_core::session::decode_pi_file(&data[..start])?
            }
        };
        let messages = resolve_pi_store_state(&decoded)?.messages;
        Ok((decoded, messages))
    }

    /// Builds a store around an already-verified descriptor. The descriptor is
    /// consumed either way: on failure it is dropped and closed. `lease` is
    /// the lease [`Prepared::activate`](super::prepared::Prepared::activate)
    /// acquired opening `path`, when the session is lease-managed, together
    /// with whether the acquisition took over another holder's epoch; the
    /// resulting store owns that lease (releases it on
    /// [`Store::close`]/drop) and records a [`Recovery`] when it does, or when the file was left mid-turn.
    pub(crate) fn from_file(
        mut file: Box<dyn JournalFile>,
        path: &str,
        lease: Option<(Arc<lease::Lease>, lease::Acquired)>,
        driver: Arc<dyn FileDriver>,
        keeper: Arc<Keeper>,
        replayable: fn(&str) -> bool,
    ) -> Result<(Self, Vec<Warning>), PiError> {
        // Until the store exists, nothing else releases `lease`: a failure
        // here must release it, or the next open waits out the lease period.
        let decoded_file = (|| {
            file.lock_exclusive()
                .map_err(|error| PiError::other(format!("lock session file: {error}")))?;
            reject_oversized_session_file(file.as_ref())?;
            let (decoded, mut warnings) = decode_pi_file_for_open(file.as_mut(), path)?;
            let mut state = resolve_pi_store_state(&decoded)?;
            warnings.extend(std::mem::take(&mut state.warnings));
            let position = file
                .seek(SeekFrom::End(0))
                .map_err(|error| PiError::other(format!("seek session file: {error}")))?;
            Ok::<_, PiError>((decoded, warnings, state, position))
        })();
        let (decoded, mut warnings, state, position) = match decoded_file {
            Ok(parts) => parts,
            Err(error) => {
                if let Some((lease, _)) = lease.as_ref() {
                    let _ = lease.release();
                }
                return Err(error);
            }
        };

        let (lease_value, acquired) = match lease {
            Some((lease, acquired)) => (Some(lease), Some(acquired)),
            None => (None, None),
        };
        let interrupted_turn = state.interrupted_turn.clone();
        let owns_lease = lease_value.is_some();
        let writer_ownership = lease_value
            .as_ref()
            .map(|lease| Arc::clone(lease) as Arc<dyn crate::session::WriterOwnership>);

        let store = Self {
            state: Mutex::new(StoreState {
                header: state.header,
                root: PathBuf::new(),
                messages: state.messages,
                input_ids: state.input_ids,
                operation_ledger: state.operation_ledger,
                aggregate_usage: state.aggregate_usage,
                usage_present: state.usage_present,
                latest_compaction: state.latest_compaction,
                thinking_level: state.thinking_level,
                session_name: state.session_name,
                forked_from: otto_core::session::fork_origin(&decoded.entries),
                plan: state.plan,
                entries: decoded.entries.clone(),
                entry_ids: state.entry_ids,
                leaf_id: state.leaf_id,
                path: path.to_owned(),
                file: Some(file),
                file_driver: FileDriverRef(driver),
                keeper: KeeperRef(keeper),
                file_bytes: position as i64,
                fatal: None,
                closed: false,
                file_path: None,
                parent_session: None,
                lease: lease_value,
                writer_ownership,
                owns_lease,
                enable_failover: None,
                recovery: None,
                deferred_custom_entries: Vec::new(),
            }),
        };
        let mut recovery = Recovery {
            cause: RecoveryCause::ProcessLost,
            repaired: Vec::new(),
            replayable: Vec::new(),
            interrupted_turn,
        };
        let replayable: fn(&str) -> bool = if recovery.resume_exhausted() {
            |_| false
        } else {
            replayable
        };
        let (repair_warnings, repaired, deferred) = store.repair_dangling_tool_calls(replayable)?;
        warnings.extend(repair_warnings);
        recovery.repaired = repaired;
        recovery.replayable = deferred;
        let recovered = match acquired {
            Some(lease::Acquired::TakenOver(holder)) => {
                recovery.cause = RecoveryCause::TakenOver(holder);
                true
            }
            _ => {
                !recovery.repaired.is_empty()
                    || !recovery.replayable.is_empty()
                    || recovery.interrupted_turn.is_some()
            }
        };
        if recovered {
            store.lock()?.recovery = Some(recovery);
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
}
