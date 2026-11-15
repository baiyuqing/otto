//! Presentation of a record's revision history: the revision list and the
//! comparison of two revisions, shared by `otto memory` and `/memory`.
//!
//! Pure functions over records; no store access.

use super::{RESTORED_FROM_REVISION_METADATA, Record, RecordHistory};

/// One line per revision, ascending, then one line per skipped history file.
pub fn render_history(history: &RecordHistory) -> String {
    let current = history.revisions.last().map(|record| record.revision);
    let mut lines: Vec<String> = history
        .revisions
        .iter()
        .map(|record| {
            let mut line = format!(
                "revision={} updated_at={} origin={} state={}",
                record.revision,
                record.updated_at,
                record
                    .source
                    .origin
                    .map(|origin| origin.as_str())
                    .unwrap_or(""),
                record.state.as_str()
            );
            if !record.superseded_by.is_empty() {
                line.push_str(&format!(" superseded_by={}", record.superseded_by));
            }
            if let Some(from) = record.metadata.get(RESTORED_FROM_REVISION_METADATA) {
                line.push_str(&format!(" restored_from={from}"));
            }
            if Some(record.revision) == current {
                line.push_str(" current=yes");
            }
            line.push_str(&format!(" text={}", record.text));
            line
        })
        .collect();
    for skipped in &history.skipped {
        lines.push(format!("skipped {}: {}", skipped.file, skipped.reason));
    }
    lines.join("\n")
}

/// The changed scalar fields and a line diff of the text between two
/// revisions of one record.
pub fn render_comparison(from: &Record, to: &Record) -> String {
    let mut lines = vec![format!(
        "revision {} ({}) -> revision {} ({})",
        from.revision, from.updated_at, to.revision, to.updated_at
    )];
    for change in field_changes(from, to) {
        lines.push(format!(
            "{}: {} -> {}",
            change.field, change.from, change.to
        ));
    }
    if from.text != to.text {
        lines.push("text:".into());
        for change in line_changes(&from.text, &to.text) {
            lines.push(match change {
                LineChange::Kept(line) => format!("  {line}"),
                LineChange::Removed(line) => format!("- {line}"),
                LineChange::Added(line) => format!("+ {line}"),
            });
        }
    }
    lines.join("\n")
}

/// A scalar field that differs between two revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FieldChange {
    pub field: &'static str,
    pub from: String,
    pub to: String,
}

/// The scalar fields that differ, in a fixed order. The text is compared by
/// [`line_changes`].
pub(crate) fn field_changes(from: &Record, to: &Record) -> Vec<FieldChange> {
    let mut changes = Vec::new();
    let mut compare = |field: &'static str, left: String, right: String| {
        if left != right {
            changes.push(FieldChange {
                field,
                from: left,
                to: right,
            });
        }
    };
    compare("kind", from.kind.clone(), to.kind.clone());
    compare("key", from.key.clone(), to.key.clone());
    // Debug form keeps `["a,b"]` distinct from `["a", "b"]`; labels may hold commas.
    compare(
        "labels",
        format!("{:?}", from.labels),
        format!("{:?}", to.labels),
    );
    compare(
        "confidence",
        from.confidence.to_string(),
        to.confidence.to_string(),
    );
    compare(
        "expires_at",
        from.expires_at
            .map(|time| time.to_string())
            .unwrap_or_default(),
        to.expires_at
            .map(|time| time.to_string())
            .unwrap_or_default(),
    );
    compare(
        "metadata",
        format!("{:?}", from.metadata),
        format!("{:?}", to.metadata),
    );
    compare(
        "state",
        from.state.as_str().to_string(),
        to.state.as_str().to_string(),
    );
    compare(
        "superseded_by",
        from.superseded_by.clone(),
        to.superseded_by.clone(),
    );
    compare(
        "state_reason",
        from.state_reason.clone(),
        to.state_reason.clone(),
    );
    compare(
        "verified_at",
        from.verified_at
            .map(|time| time.to_string())
            .unwrap_or_default(),
        to.verified_at
            .map(|time| time.to_string())
            .unwrap_or_default(),
    );
    compare(
        "review_at",
        from.review_at
            .map(|time| time.to_string())
            .unwrap_or_default(),
        to.review_at
            .map(|time| time.to_string())
            .unwrap_or_default(),
    );
    changes
}

/// One line of a line diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineChange {
    Kept(String),
    Removed(String),
    Added(String),
}

/// A line diff by longest common subsequence: kept lines in order, removed
/// lines before the added lines that replace them. Record text is at most
/// 8 KiB, so the quadratic table is small.
pub(crate) fn line_changes(from: &str, to: &str) -> Vec<LineChange> {
    let from_lines: Vec<&str> = from.lines().collect();
    let to_lines: Vec<&str> = to.lines().collect();
    let rows = from_lines.len();
    let columns = to_lines.len();
    // common[i][j] = length of the longest common subsequence of
    // from_lines[i..] and to_lines[j..].
    let mut common = vec![vec![0usize; columns + 1]; rows + 1];
    for i in (0..rows).rev() {
        for j in (0..columns).rev() {
            common[i][j] = if from_lines[i] == to_lines[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    let mut changes = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < rows && j < columns {
        if from_lines[i] == to_lines[j] {
            changes.push(LineChange::Kept(from_lines[i].to_owned()));
            i += 1;
            j += 1;
        } else if common[i + 1][j] >= common[i][j + 1] {
            changes.push(LineChange::Removed(from_lines[i].to_owned()));
            i += 1;
        } else {
            changes.push(LineChange::Added(to_lines[j].to_owned()));
            j += 1;
        }
    }
    changes.extend(
        from_lines[i..]
            .iter()
            .map(|line| LineChange::Removed((*line).to_owned())),
    );
    changes.extend(
        to_lines[j..]
            .iter()
            .map(|line| LineChange::Added((*line).to_owned())),
    );
    changes
}

#[cfg(test)]
mod tests;
