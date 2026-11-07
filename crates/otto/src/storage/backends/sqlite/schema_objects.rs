//! Comparison of SQLite schema objects ignoring formatting differences.
use crate::storage::{Error, Result};
use std::collections::BTreeMap;

/// Normalizes one statement: trim, drop one trailing semicolon, remove
/// whitespace outside single-quoted values.
pub(crate) fn normalize_schema_sql(statement: &str) -> String {
    // Schemas written by the earlier Turso engine serialize parsed SQL: keyword
    // identifiers gain quotes, <> becomes !=, and punctuation spacing changes.
    // Compare schemas ignoring that; preserve every quoted value.
    statement
        .trim()
        .trim_end_matches(';')
        .split('\'')
        .enumerate()
        .map(|(index, part)| {
            if index % 2 == 1 {
                return part.to_string();
            }
            part.replace("\"key\"", "key")
                .replace("\"action\"", "action")
                .replace("<>", "!=")
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect()
        })
        .collect::<Vec<_>>()
        .join("'")
}

/// Succeeds only when both lists hold the same objects after normalization.
pub(crate) fn validate_objects(source: &[String], canonical: &[String]) -> Result<()> {
    fn objects(objects: &[String]) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for object in objects {
            *counts.entry(normalize_schema_sql(object)).or_insert(0) += 1;
        }
        counts
    }
    if objects(source) != objects(canonical) {
        return Err(Error::Misuse("unsupported schema".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_differences_are_ignored_and_quoted_values_are_kept() {
        let canonical = vec!["CREATE TABLE a (x TEXT CHECK (x <> 'a b'));".to_string()];
        let reformatted = vec!["CREATE TABLE a(x TEXT CHECK(x != 'a b'))".to_string()];
        validate_objects(&reformatted, &canonical).unwrap();
        let altered_value = vec!["CREATE TABLE a(x TEXT CHECK(x != 'ab'))".to_string()];
        assert!(validate_objects(&altered_value, &canonical).is_err());
    }

    #[test]
    fn extra_and_missing_objects_are_rejected() {
        let canonical = vec!["CREATE TABLE a(x TEXT)".to_string()];
        let mut extra = canonical.clone();
        extra.push("CREATE INDEX unexpected ON a(lower(x))".into());
        assert!(validate_objects(&extra, &canonical).is_err());
        assert!(validate_objects(&[], &canonical).is_err());
        validate_objects(&canonical, &canonical).unwrap();
    }
}
