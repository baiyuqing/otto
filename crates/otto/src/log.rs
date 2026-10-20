//! The process log: `time=<RFC 3339> level=<LEVEL> msg=<event> key=value ...` lines.

use std::sync::{Arc, Mutex, OnceLock};
mod diagnostics;

/// ponytail: no logging crate is pinned and the server writes a handful of
/// fixed lines, so a small formatter beats adding `tracing` plus a subscriber.
/// Swap for `tracing` if any other crate needs structured logs.
pub struct Logger {
    diagnostics: OnceLock<Arc<diagnostics::Log>>,
    sink: Mutex<Box<dyn std::io::Write + Send>>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Logger")
    }
}

impl Logger {
    pub fn stderr() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }

    /// Suppresses ordinary stderr lines for terminal frontends. Startup can
    /// still enable the separate content-free diagnostic file sink.
    pub fn discard() -> Self {
        Self::new(Box::new(std::io::sink()))
    }

    pub fn new(sink: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            diagnostics: OnceLock::new(),
            sink: Mutex::new(sink),
        }
    }

    /// Enables a separate content-free file sink, including on terminal-safe
    /// discard loggers. Ordinary stderr lines are never copied into this file.
    pub fn enable_diagnostics(&self, home: &std::path::Path) -> std::io::Result<()> {
        if self.diagnostics.get().is_none() {
            let log = diagnostics::Log::open(home)?;
            let _ = self.diagnostics.set(log);
        }
        Ok(())
    }

    pub fn diagnostic_scope(
        &self,
        session: &str,
        task: &str,
        redactor: otto_core::agent::redactor::Redactor,
    ) -> Option<Arc<dyn otto_core::diagnostics::Diagnostics>> {
        self.diagnostics
            .get()
            .map(|log| log.scope(session, task, redactor))
    }

    /// Content-free reflection decision measurements, also available on terminal
    /// discard loggers. Callers supply only fixed enums and numeric strings.
    pub(crate) fn reflection_decision(&self, fields: &[(&str, String)]) {
        self.info("reflection_decision", fields);
        if let Some(log) = self.diagnostics.get() {
            log.reflection_decision(fields);
        }
    }

    pub fn info(&self, message: &str, fields: &[(&str, String)]) {
        self.write("INFO", message, fields);
    }

    pub fn warn(&self, message: &str, fields: &[(&str, String)]) {
        self.write("WARN", message, fields);
    }

    pub fn error(&self, message: &str, fields: &[(&str, String)]) {
        self.write("ERROR", message, fields);
    }

    fn write(&self, level: &str, message: &str, fields: &[(&str, String)]) {
        let mut line = format!(
            "time={} level={level} msg={}",
            chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            quote(message)
        );
        for (key, value) in fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(&quote(value));
        }
        line.push('\n');
        let mut sink = self
            .sink
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = sink.write_all(line.as_bytes());
        let _ = sink.flush();
    }
}

/// The log quoting rule: quote an empty value, and any value carrying a space,
/// a quote, an equals sign, a backslash, or a control character.
fn quote(value: &str) -> String {
    let unsafe_byte = value.is_empty()
        || value
            .chars()
            .any(|c| c <= ' ' || c == '"' || c == '=' || c == '\\' || c == '\u{7f}');
    if unsafe_byte {
        serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
    } else {
        value.to_string()
    }
}

/// Reduces a client-supplied `X-Request-ID` or ACP `_meta.otto.requestId` to at most 64 printable ASCII
/// bytes, per the design's trust-and-safety rule.
pub(crate) fn request_id(raw: &[u8]) -> String {
    raw.iter()
        .take(64)
        .filter(|byte| (0x20..0x7f).contains(*byte))
        .map(|&byte| byte as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn levels_and_quoting_keep_the_line_format() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let log = Logger::new(Box::new(Sink(Arc::clone(&bytes))));
        log.warn("slow", &[("detail", "a b".to_string())]);
        log.info("ok", &[]);
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert!(
            lines[0].contains(" level=WARN msg=slow detail=\"a b\""),
            "{text}"
        );
        assert!(lines[1].contains(" level=INFO msg=ok"), "{text}");
    }

    #[test]
    fn discard_writes_nothing_and_does_not_fail() {
        Logger::discard().error("x", &[("k", "v".to_string())]);
    }

    #[test]
    fn request_id_keeps_printable_ascii_up_to_64_bytes() {
        assert_eq!(request_id(b"ab\x01\x7fc d"), "abc d");
        assert_eq!(request_id(&[b'a'; 100]).len(), 64);
        assert_eq!(request_id(b"\x01\x02"), "");
    }
}
