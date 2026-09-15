//! Source guard for the tool argument convention.
//!
//! Models emit an empty list in the position of an argument they are not
//! using, next to the arguments they are using. A tool that reads `[]` as a
//! present key rejects calls it can otherwise serve, and the model has no way
//! to retry: it already sent the shape it can produce. The convention is that
//! an optional list argument reads an empty list as an absent key, which
//! `otto::tool::empty_as_none` implements once.
//!
//! The scan is a substring match over the tool sources rather than a parse of
//! them. A future `Option<Vec<_>>` that is not a deserialized tool argument
//! would fail here and needs an exemption added to `EXEMPT`, not a silenced
//! assertion.

use std::path::Path;

/// Field declarations that are not tool arguments, as `file:field`.
const EXEMPT: &[&str] = &[];

/// Attribute and doc lines directly above a field, nearest first.
fn attributes_above(lines: &[&str], field: usize) -> Vec<String> {
    lines[..field]
        .iter()
        .rev()
        .take_while(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("#[") || trimmed.starts_with("///")
        })
        .map(|line| line.trim().to_owned())
        .collect()
}

#[test]
fn every_optional_list_argument_reads_an_empty_list_as_absent() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tool");
    let mut scanned = 0usize;
    let mut offenders = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("the tool module directory is readable")
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no tool sources found in {dir:?}");

    for path in entries {
        let source = std::fs::read_to_string(&path).expect("a readable tool source");
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let lines: Vec<&str> = source.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if !trimmed.contains(": Option<Vec<") || !trimmed.ends_with(',') {
                continue;
            }
            let name = trimmed.split(':').next().unwrap_or(trimmed).trim();
            if EXEMPT.contains(&format!("{file}:{name}").as_str()) {
                continue;
            }
            scanned += 1;
            if !attributes_above(&lines, index)
                .iter()
                .any(|attribute| attribute.contains("empty_as_none"))
            {
                offenders.push(format!("{file}:{} {name}", index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "optional list arguments must deserialize with `empty_as_none`, so that an \
         empty list reads as an absent key: {offenders:?}"
    );
    assert!(scanned > 0, "the scan matched no optional list arguments");
}
