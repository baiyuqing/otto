//! The output directory. Every text written here passes through the redactor.

use std::io::Write;
use std::path::{Path, PathBuf};

use otto_core::agent::redactor::Redactor;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct OutputDir {
    root: PathBuf,
    redactor: Redactor,
}

impl OutputDir {
    /// Creates the directory (and parents) if it does not exist.
    pub fn create(root: &Path, redactor: Redactor) -> Result<Self, String> {
        std::fs::create_dir_all(root)
            .map_err(|error| format!("create output directory {}: {error}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            redactor,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.path(name).is_file()
    }

    /// The JSON text of `value` with every string redacted; keys included.
    fn redacted_json<T: Serialize>(&self, value: &T) -> Result<serde_json::Value, String> {
        let raw = serde_json::to_string(value).map_err(|error| format!("encode JSON: {error}"))?;
        let redacted = self.redactor.redact_json_strings(&raw);
        serde_json::from_str(redacted.get())
            .map_err(|error| format!("encode redacted JSON: {error}"))
    }

    /// Writes `value` as pretty JSON.
    pub fn write_json<T: Serialize>(&self, name: &str, value: &T) -> Result<(), String> {
        let json = self.redacted_json(value)?;
        let text =
            serde_json::to_string_pretty(&json).map_err(|error| format!("encode JSON: {error}"))?;
        self.write_file(name, &(text + "\n"))
    }

    /// Writes text such as Markdown.
    pub fn write_text(&self, name: &str, text: &str) -> Result<(), String> {
        self.write_file(name, &self.redactor.redact_string(text))
    }

    /// Replaces the file by one JSON line per item.
    pub fn write_jsonl<T: Serialize>(&self, name: &str, items: &[T]) -> Result<(), String> {
        let mut text = String::new();
        for item in items {
            text.push_str(&self.redacted_json(item)?.to_string());
            text.push('\n');
        }
        self.write_file(name, &text)
    }

    /// Appends one JSON line and flushes it.
    pub fn append_jsonl<T: Serialize>(&self, name: &str, item: &T) -> Result<(), String> {
        let mut line = self.redacted_json(item)?.to_string();
        line.push('\n');
        let path = self.path(name);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|error| format!("write {}: {error}", path.display()))
    }

    /// Reads a JSON-lines file; a missing file is an empty list.
    pub fn read_jsonl<T: DeserializeOwned>(&self, name: &str) -> Result<Vec<T>, String> {
        let path = self.path(name);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(index, line)| {
                serde_json::from_str(line)
                    .map_err(|error| format!("{} line {}: {error}", path.display(), index + 1))
            })
            .collect()
    }

    /// Reads a JSON file.
    pub fn read_json<T: DeserializeOwned>(&self, name: &str) -> Result<T, String> {
        let path = self.path(name);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn write_file(&self, name: &str, text: &str) -> Result<(), String> {
        let path = self.path(name);
        std::fs::write(&path, text).map_err(|error| format!("write {}: {error}", path.display()))
    }
}
