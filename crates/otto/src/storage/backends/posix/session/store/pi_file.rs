//! Pi file record writing, header validation and decoding.

use super::*;

/// Writes `encoded` plus its newline delimiter and `fsync`s the file.
pub(super) fn write_pi_record(file: &mut dyn JournalFile, encoded: &[u8]) -> Result<i64, PiError> {
    let mut record = Vec::with_capacity(encoded.len() + 1);
    record.extend_from_slice(encoded);
    record.push(b'\n');
    file.write_all(&record)
        .map_err(|error| PiError::other(format!("write session record: {error}")))?;
    file.sync_all()
        .map_err(|error| PiError::other(format!("sync session file: {error}")))?;
    Ok(record.len() as i64)
}

pub(super) fn raw_value(text: String) -> Result<Box<serde_json::value::RawValue>, PiError> {
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
    pub(crate) input_ids: HashSet<String>,
    pub(crate) aggregate_usage: Usage,
    pub(crate) usage_present: bool,
    pub(crate) latest_compaction: Option<CompactionMetadata>,
    pub(crate) thinking_level: String,
    pub(crate) session_name: Option<String>,
    pub(crate) entry_ids: HashSet<String>,
    pub(crate) leaf_id: Option<String>,
    pub(crate) operation_ledger: OperationLedger,
    pub(crate) interrupted_turn: Option<otto_core::session::InterruptedTurn>,
    pub(crate) plan: Vec<otto_core::session::PlanStep>,
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
    let mut latest_compaction = latest_compaction_metadata(&decoded.entries, &leaf)?;
    if let Some(metadata) = latest_compaction.as_mut() {
        project_compaction_metadata(metadata, &decoded.entries, &leaf, &resolved.model_messages)?;
    }
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
        input_ids: resolved.input_ids,
        aggregate_usage: resolved.usage,
        usage_present: resolved.usage_present,
        latest_compaction,
        thinking_level,
        session_name,
        entry_ids,
        leaf_id,
        operation_ledger: resolved.operation_ledger,
        interrupted_turn: resolved.interrupted_turn,
        plan: resolved.plan,
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

pub(super) fn normalize_session_thinking(thinking: &str) -> Result<String, PiError> {
    match thinking {
        "" | "low" | "medium" | "high" | "xhigh" | "max" => Ok(thinking.to_string()),
        _ => Err(PiError::invalid(
            "invalid thinking: must be one of low, medium, high, xhigh, max",
        )),
    }
}

pub(super) fn thinking_to_pi_level(thinking: &str) -> String {
    if thinking.is_empty() {
        "off".to_string()
    } else {
        thinking.to_string()
    }
}

pub(super) fn pi_level_to_thinking(thinking: &str) -> Result<String, PiError> {
    match thinking {
        "" | "off" => Ok(String::new()),
        "low" | "medium" | "high" | "xhigh" | "max" => Ok(thinking.to_string()),
        _ => Err(PiError::invalid(
            "invalid thinking level: must be one of off, low, medium, high, xhigh, max",
        )),
    }
}

/// Rejects a file above [`MAX_SESSION_FILE_BYTES`] before any of it is read.
pub(crate) fn reject_oversized_session_file(file: &dyn JournalFile) -> Result<(), PiError> {
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
pub(super) fn final_pi_record_state(
    file: &mut dyn JournalFile,
) -> Result<(u64, bool, bool), PiError> {
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

pub(super) fn read_exact_at(
    file: &mut dyn JournalFile,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)
}

/// True when the line is valid JSON that simply stops early, which is what a
/// process killed mid-append leaves behind.
pub(super) fn is_incomplete_json(line: &[u8]) -> bool {
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(_) => false,
        Err(error) => error.is_eof(),
    }
}

pub(super) fn read_all(
    file: &mut dyn JournalFile,
    start: u64,
    end: Option<u64>,
) -> Result<Vec<u8>, PiError> {
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
pub(crate) fn decode_pi_file_read_only(file: &mut dyn JournalFile) -> Result<PiFile, PiError> {
    let (final_start, _, incomplete) = final_pi_record_state(file)?;
    let end = (incomplete && final_start > 0).then_some(final_start);
    decode_pi_file(&read_all(file, 0, end)?)
}

/// Decodes a session file for appending, repairing a truncated final record or
/// a missing final newline. Both repairs are rejected when the repaired file
/// would not resolve to a valid session.
pub(super) fn decode_pi_file_for_open(
    file: &mut dyn JournalFile,
    path: &str,
) -> Result<(PiFile, Vec<Warning>), PiError> {
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
