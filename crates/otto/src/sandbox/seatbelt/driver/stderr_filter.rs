//! Bounded output collection and `sandbox-exec` diagnostic filtering.

use super::*;

/// A writer that keeps at most `limit` bytes and remembers if more arrived.
pub(super) struct BoundedCollector {
    limit: usize,
    buffer: Vec<u8>,
    overflow: bool,
}

impl BoundedCollector {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            buffer: Vec::new(),
            overflow: false,
        }
    }

    pub(super) fn overflowed(&self) -> bool {
        self.overflow
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.buffer
    }
}

impl std::io::Write for BoundedCollector {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.buffer.len());
        if remaining < data.len() {
            self.overflow = true;
        }
        self.buffer
            .extend_from_slice(&data[..remaining.min(data.len())]);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Separates `sandbox-exec`'s own diagnostics from the child's stderr.
///
/// Bytes are buffered only while the first line could still turn out to be a
/// `sandbox-exec:` diagnostic, at most [`STDERR_DECISION_LIMIT`] of them. Once
/// the output is known to be ordinary the buffer is flushed and every later
/// write passes straight through.
pub(super) struct StderrFilter<'a> {
    destination: &'a mut (dyn std::io::Write + Send),
    pending: Vec<u8>,
    ordinary: bool,
    infrastructure: bool,
    execvp_suppressed: bool,
    write_failed: bool,
    on_infrastructure: Option<&'a (dyn Fn() + Sync)>,
}

impl<'a> StderrFilter<'a> {
    pub(super) fn new(
        destination: &'a mut (dyn std::io::Write + Send),
        on_infrastructure: Option<&'a (dyn Fn() + Sync)>,
    ) -> Self {
        Self {
            destination,
            pending: Vec::new(),
            ordinary: false,
            infrastructure: false,
            execvp_suppressed: false,
            write_failed: false,
            on_infrastructure,
        }
    }

    /// Flushes whatever is still buffered and reports whether the caller's
    /// writer failed. A withheld diagnostic is discarded, not flushed.
    pub(super) fn finish(&mut self) -> Result<(), Error> {
        if !self.ordinary
            && !self.infrastructure
            && !self.execvp_suppressed
            && !self.pending.is_empty()
        {
            self.classify_pending();
        }
        if self.infrastructure || self.execvp_suppressed {
            self.pending.clear();
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        if self.write_ordinary(&pending).is_err() || self.write_failed {
            return Err(Error::ChildWait);
        }
        Ok(())
    }

    pub(super) fn infrastructure_failure(&self) -> bool {
        self.infrastructure
    }

    fn classify_pending(&mut self) {
        if self.pending.starts_with(POLICY_EXEC_PREFIX) {
            self.execvp_suppressed = true;
            self.pending.clear();
            return;
        }
        if self.pending.starts_with(DIAGNOSTIC_PREFIX) {
            if let Some(latch) = self.on_infrastructure {
                latch();
            }
            self.infrastructure = true;
            self.pending.clear();
            return;
        }
        self.ordinary = true;
    }

    fn write_ordinary(&mut self, data: &[u8]) -> std::io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if let Err(error) = self.destination.write_all(data) {
            self.write_failed = true;
            return Err(error);
        }
        Ok(())
    }
}

impl std::io::Write for StderrFilter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let original = data.len();
        if self.infrastructure || self.execvp_suppressed {
            return Ok(original);
        }
        if self.ordinary {
            self.write_ordinary(data)?;
            return Ok(original);
        }

        let mut unclassified_bytes = data;
        while !unclassified_bytes.is_empty()
            && !self.ordinary
            && !self.infrastructure
            && !self.execvp_suppressed
        {
            let remaining = STDERR_DECISION_LIMIT.saturating_sub(self.pending.len());
            if remaining == 0 {
                self.classify_pending();
                break;
            }
            let mut take = unclassified_bytes.len().min(remaining);
            if let Some(newline) = unclassified_bytes[..take]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                take = newline + 1;
            }
            self.pending.extend_from_slice(&unclassified_bytes[..take]);
            unclassified_bytes = &unclassified_bytes[take..];

            if !could_begin_diagnostic(&self.pending) {
                self.ordinary = true;
            } else if self.pending.contains(&b'\n') || self.pending.len() == STDERR_DECISION_LIMIT {
                self.classify_pending();
            }
        }
        if self.infrastructure || self.execvp_suppressed {
            return Ok(original);
        }
        if self.ordinary {
            let pending = std::mem::take(&mut self.pending);
            self.write_ordinary(&pending)?;
            self.write_ordinary(unclassified_bytes)?;
        }
        Ok(original)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Whether `value` is still a possible prefix of a `sandbox-exec:` diagnostic.
fn could_begin_diagnostic(value: &[u8]) -> bool {
    if value.len() <= DIAGNOSTIC_PREFIX.len() {
        value == &DIAGNOSTIC_PREFIX[..value.len()]
    } else {
        value.starts_with(DIAGNOSTIC_PREFIX)
    }
}
