//! Task name validation and task id counter recovery.

/// Whether `name` is `^[A-Za-z0-9][A-Za-z0-9_-]*$`.
pub(super) fn is_task_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    characters
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
}

/// Whether `name` has the shape of an auto-assigned task id, `^t[0-9]+$`.
pub(super) fn is_reserved_task_name(name: &str) -> bool {
    match name.strip_prefix('t') {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// The highest task counter used by a child transcript file name
/// `t<counter>-<random id>.jsonl` in `dir`, or 0 if `dir` does not exist or
/// has no matching name. Names that do not match are ignored. Pass the
/// result as `starting_counter` to [`Tasks::with_recorder_from`] so a
/// resumed session's task ids do not collide with a prior run's.
#[cfg(test)]
pub fn highest_task_counter(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut highest = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix('t') else {
            continue;
        };
        let Some(digits) = rest.split('-').next() else {
            continue;
        };
        if digits.is_empty() || !rest[digits.len()..].starts_with('-') {
            continue;
        }
        if let Ok(counter) = digits.parse::<u64>() {
            highest = highest.max(counter);
        }
    }
    highest
}
