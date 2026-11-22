//! Private rotating metadata logs. Each runtime owns an exclusive random family
//! and an advisory lock; cleanup never touches an active family's files.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::{Duration, Instant, SystemTime};

use crate::fsops;
use otto_core::agent::redactor::Redactor;
use otto_core::diagnostics::{Diagnostics, Finish, Kind, Observer, Phase, RetryReason};
use serde_json::{Value, json};

const SEGMENT_BYTES: u64 = 5 * 1024 * 1024;
const BACKUPS: usize = 3;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HEARTBEAT: Duration = Duration::from_secs(15);

struct Writer {
    dir: fsops::Dir,
    prefix: String,
    file: File,
    _owner: File,
    size: u64,
    limit: u64,
    dropped: u64,
    reopen: bool,
    version: &'static str,
}

fn regular(file: File) -> io::Result<File> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular diagnostic file"));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
fn create(dir: &fsops::Dir, name: &str, exclusive: bool) -> io::Result<File> {
    regular(fsops::create_at_no_follow(
        dir,
        name,
        libc::O_WRONLY
            | libc::O_APPEND
            | libc::O_NONBLOCK
            | if exclusive { libc::O_EXCL } else { 0 },
        0o600,
    )?)
}
fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
fn family(lock: &str) -> Option<&str> {
    let prefix = lock.strip_suffix(".lock")?;
    let parts: Vec<_> = prefix.split('-').collect();
    (parts.len() == 3
        && parts[0] == "diagnostic"
        && !parts[1].is_empty()
        && parts[1].bytes().all(|c| c.is_ascii_digit())
        && parts[2].len() == 32
        && parts[2].bytes().all(|c| c.is_ascii_hexdigit()))
    .then_some(prefix)
}
fn cleanup(dir: &fsops::Dir, now: SystemTime, retention: Duration) {
    let Ok(names) = fsops::read_directory_names(dir, 10000) else {
        return;
    };
    for name in names {
        let Some(prefix) = family(&name) else {
            continue;
        };
        let Ok(owner) = fsops::open_at_no_follow(dir, &name, libc::O_RDWR | libc::O_NONBLOCK)
        else {
            continue;
        };
        if !owner.metadata().is_ok_and(|m| m.is_file())
            || fsops::try_lock_exclusive(&owner).is_err()
        {
            continue;
        }
        let mut latest = owner.metadata().and_then(|m| m.modified()).unwrap_or(now);
        for suffix in [".jsonl", ".jsonl.1", ".jsonl.2", ".jsonl.3"] {
            if let Ok(file) = fsops::open_at_no_follow(
                dir,
                &format!("{prefix}{suffix}"),
                libc::O_RDONLY | libc::O_NONBLOCK,
            ) && let Ok(modified) = file.metadata().and_then(|m| m.modified())
            {
                latest = latest.max(modified);
            }
        }
        if now.duration_since(latest).is_ok_and(|age| age >= retention) {
            let mut removed = true;
            for suffix in [".jsonl", ".jsonl.1", ".jsonl.2", ".jsonl.3"] {
                removed &=
                    ignore_missing(fsops::unlink_at(dir, &format!("{prefix}{suffix}"))).is_ok();
            }
            if removed {
                let _ = fsops::unlink_at(dir, &name);
            }
        }
    }
}

/// Locks the owner file of a new family. Another process's `cleanup` may hold
/// a non-blocking probe lock on the freshly created file for a moment, so this
/// retries for the grace period of `fsops::lock_exclusive` before failing.
fn lock_new_owner(owner: &File) -> io::Result<()> {
    fsops::lock_exclusive(owner)
}

impl Writer {
    fn open(home: &Path, version: &'static str) -> io::Result<Self> {
        let home = fsops::open_dir_no_follow(&std::fs::canonicalize(home)?)?;
        match fsops::mkdir_at(&home, ".otto", 0o700) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let otto = fsops::open_dir_at_no_follow(&home, ".otto")?;
        match fsops::mkdir_at(&otto, "logs", 0o700) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let dir = fsops::open_dir_at_no_follow(&otto, "logs")?;
        fsops::fchmod_dir(&dir)?;
        cleanup(&dir, SystemTime::now(), RETENTION);
        let id = crate::random::random_hex::<16>().map_err(io::Error::other)?;
        let prefix = format!("diagnostic-{}-{id}", std::process::id());
        let owner = regular(fsops::create_at_no_follow(
            &dir,
            &format!("{prefix}.lock"),
            libc::O_RDWR | libc::O_EXCL | libc::O_NONBLOCK,
            0o600,
        )?)?;
        lock_new_owner(&owner)?;
        let file = create(&dir, &format!("{prefix}.jsonl"), true)?;
        Ok(Self {
            dir,
            prefix,
            file,
            _owner: owner,
            size: 0,
            limit: SEGMENT_BYTES,
            dropped: 0,
            reopen: false,
            version,
        })
    }
    fn write(&mut self, mut record: Value) -> io::Result<()> {
        record["time"] =
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        record["pid"] = json!(std::process::id());
        record["instance"] = json!(self.prefix);
        record["version"] = json!(self.version);
        record["dropped_records"] = json!(self.dropped);
        let mut bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let result = self.append(&bytes);
        if result.is_ok() {
            self.dropped = 0;
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
        result
    }
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.reopen {
            self.file = create(&self.dir, &format!("{}.jsonl", self.prefix), false)?;
            self.size = self.file.metadata()?.len();
            self.reopen = false;
        }
        if self.size > 0 && self.size.saturating_add(bytes.len() as u64) > self.limit {
            ignore_missing(fsops::unlink_at(
                &self.dir,
                &format!("{}.jsonl.{BACKUPS}", self.prefix),
            ))?;
            for n in (1..BACKUPS).rev() {
                ignore_missing(fsops::rename_at(
                    &self.dir,
                    &format!("{}.jsonl.{n}", self.prefix),
                    &format!("{}.jsonl.{}", self.prefix, n + 1),
                ))?;
            }
            ignore_missing(fsops::rename_at(
                &self.dir,
                &format!("{}.jsonl", self.prefix),
                &format!("{}.jsonl.1", self.prefix),
            ))?;
            self.reopen = true;
            self.file = create(&self.dir, &format!("{}.jsonl", self.prefix), false)?;
            self.reopen = false;
            self.size = 0;
        }
        if let Err(error) = self.file.write_all(bytes) {
            let _ = self.file.set_len(self.size);
            return Err(error);
        }
        self.file.flush()?;
        self.size = self.size.saturating_add(bytes.len() as u64);
        Ok(())
    }
}

pub(super) struct Log {
    writer: Mutex<Writer>,
    active: Mutex<BTreeMap<u64, Weak<Span>>>,
    next: AtomicU64,
    stop: mpsc::Sender<()>,
}
impl Log {
    pub(super) fn open(home: &Path, version: &'static str) -> io::Result<Arc<Self>> {
        let mut writer = Writer::open(home, version)?;
        writer.write(json!({"event":"process_started","schema_version":1,"version":version}))?;
        let (stop, receiver) = mpsc::channel();
        let log = Arc::new(Self {
            writer: Mutex::new(writer),
            active: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            stop,
        });
        let weak = Arc::downgrade(&log);
        std::thread::Builder::new()
            .name("otto-diagnostics".into())
            .spawn(move || {
                while matches!(
                    receiver.recv_timeout(HEARTBEAT),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let Some(log) = weak.upgrade() else {
                        break;
                    };
                    log.heartbeat();
                }
            })?;
        Ok(log)
    }
    fn write(&self, record: Value) {
        let _ = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write(record);
    }
    fn heartbeat(&self) {
        let spans: Vec<_> = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for span in spans {
            let progress = span
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !progress.finished {
                span.record("heartbeat", &progress, None);
            }
        }
    }
    pub(super) fn scope(
        self: &Arc<Self>,
        session: &str,
        task: &str,
        redactor: Redactor,
        stops: Arc<super::StopListener>,
    ) -> Arc<dyn Diagnostics> {
        let redactor = Arc::new(redactor);
        Arc::new(Scope {
            stops,
            log: self.clone(),
            session: safe(session, &redactor),
            task: safe(task, &redactor),
            redactor,
        })
    }
}
impl Drop for Log {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        self.write(json!({"event":"process_stopped","schema_version":1}));
    }
}
fn safe(text: &str, redactor: &Redactor) -> String {
    redactor
        .redact_string(text)
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect()
}
struct Scope {
    stops: Arc<super::StopListener>,
    log: Arc<Log>,
    session: String,
    task: String,
    redactor: Arc<Redactor>,
}
impl Diagnostics for Scope {
    fn start(
        &self,
        kind: Kind,
        operation_id: &str,
        tool: Option<&str>,
        provider: &str,
        model: &str,
    ) -> Arc<dyn Observer> {
        let id = self.log.next.fetch_add(1, Ordering::Relaxed);
        let progress = Progress {
            phase: match kind {
                Kind::Turn => Phase::Active,
                Kind::Tool => Phase::ToolOperation,
                _ => Phase::Starting,
            },
            ..Progress::default()
        };
        let span = Arc::new(Span {
            log: self.log.clone(),
            id,
            kind,
            session: self.session.clone(),
            task: self.task.clone(),
            operation: safe(operation_id, &self.redactor),
            tool: tool.map(|name| safe(name, &self.redactor)),
            provider: safe(provider, &self.redactor),
            model: safe(model, &self.redactor),
            start: Instant::now(),
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            progress: Mutex::new(progress),
        });
        self.log
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::downgrade(&span));
        {
            let progress = span
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            span.record("operation_started", &progress, None);
        }
        span
    }
    fn turn_stalled(&self, stall: &otto_core::agent::progress::Stall) {
        self.record_stall(stall);
    }
    fn turn_budget_exceeded(&self, exceeded: &otto_core::agent::budget::Exceeded) {
        self.record_budget_exceeded(exceeded);
    }
    fn for_task(&self, task: &str) -> Option<Arc<dyn Diagnostics>> {
        Some(Arc::new(Self {
            stops: self.stops.clone(),
            log: self.log.clone(),
            session: self.session.clone(),
            task: safe(task, &self.redactor),
            redactor: self.redactor.clone(),
        }))
    }
}
struct Progress {
    phase: Phase,
    last_activity_ms: u64,
    last_network_ms: Option<u64>,
    last_model_ms: Option<u64>,
    first_network_ms: Option<u64>,
    first_model_ms: Option<u64>,
    retry_delay_ms: Option<u64>,
    last_retry_reason: Option<RetryReason>,
    network_bytes: u64,
    model_bytes: u64,
    stream_events: u64,
    http_status: Option<u16>,
    attempt: u32,
    next_attempt: Option<u32>,
    finished: bool,
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Phase::Starting,
            last_activity_ms: 0,
            last_network_ms: None,
            last_model_ms: None,
            first_network_ms: None,
            first_model_ms: None,
            retry_delay_ms: None,
            last_retry_reason: None,
            network_bytes: 0,
            model_bytes: 0,
            stream_events: 0,
            http_status: None,
            attempt: 0,
            next_attempt: None,
            finished: false,
        }
    }
}
struct Span {
    log: Arc<Log>,
    id: u64,
    kind: Kind,
    session: String,
    task: String,
    operation: String,
    tool: Option<String>,
    provider: String,
    model: String,
    start: Instant,
    started_at: String,
    progress: Mutex<Progress>,
}
impl Span {
    fn elapsed(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn record(&self, event: &'static str, p: &Progress, finish: Option<&Finish>) {
        let elapsed = self.elapsed();
        self.log.write(json!({"schema_version":1,
            "event":event,
            "span_id":self.id,
            "kind":self.kind,
            "session_id":self.session,
            "task_id":self.task,
            "operation_id":self.operation,
            "tool":self.tool,
            "provider":self.provider,
            "model":self.model,
            "started_at":self.started_at,
            "phase":p.phase,
            "first_network_ms":p.first_network_ms,
            "first_model_ms":p.first_model_ms,
            "retry_delay_ms":p.retry_delay_ms,
            "last_retry_reason":p.last_retry_reason,
            "elapsed_ms":elapsed,
            "last_activity_ms":p.last_activity_ms,
            "idle_ms":if matches!(self.kind, Kind::Turn | Kind::Tool) { None } else { Some(elapsed.saturating_sub(p.last_activity_ms)) },
            "last_network_ms":p.last_network_ms,
            "last_model_ms":p.last_model_ms,
            "network_bytes":p.network_bytes,
            "model_bytes":p.model_bytes,
            "stream_events":p.stream_events,
            "http_status":p.http_status,
            "attempt":p.attempt,
            "next_attempt":p.next_attempt,
            "finish":finish}));
    }
}
impl Observer for Span {
    fn activity(&self, phase: Phase, bytes: usize, status: Option<u16>) {
        let mut p = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.finished {
            return;
        }
        p.phase = phase;
        p.last_activity_ms = self.elapsed();
        let immediate = match phase {
            Phase::Sending => {
                p.attempt = p.next_attempt.take().unwrap_or(1);
                p.http_status = None;
                p.retry_delay_ms = None;
                true
            }
            Phase::Headers => {
                p.http_status = status;
                true
            }
            Phase::Receiving => {
                let first = p.last_network_ms.is_none();
                p.last_network_ms = Some(p.last_activity_ms);
                if first {
                    p.first_network_ms = Some(p.last_activity_ms);
                }
                p.network_bytes = p.network_bytes.saturating_add(bytes as u64);
                first
            }
            Phase::StreamingText | Phase::StreamingReasoning | Phase::StreamingTool => {
                let first = p.last_model_ms.is_none();
                p.last_model_ms = Some(p.last_activity_ms);
                if first {
                    p.first_model_ms = Some(p.last_activity_ms);
                }
                p.model_bytes = p.model_bytes.saturating_add(bytes as u64);
                p.stream_events = p.stream_events.saturating_add(1);
                first
            }
            _ => false,
        };
        if immediate {
            self.record("activity", &p, None);
        }
    }
    fn retry(&self, attempt: u32, delay_ms: u64, reason: RetryReason) {
        let mut p = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.finished {
            return;
        }
        p.phase = Phase::RetryWait;
        p.last_activity_ms = self.elapsed();
        p.next_attempt = Some(attempt);
        p.retry_delay_ms = Some(delay_ms);
        p.last_retry_reason = Some(reason);
        self.record("provider_retry", &p, None);
    }
    fn finish(&self, finish: Finish) {
        let mut p = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.finished {
            return;
        }
        p.finished = true;
        self.record("operation_finished", &p, Some(&finish));
        self.log
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
    fn abandoned(&self) {
        let mut p = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.finished {
            return;
        }
        p.finished = true;
        self.record("operation_abandoned", &p, None);
        self.log
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.abandoned();
    }
}

mod stops;

#[cfg(test)]
mod budget_tests;

#[cfg(test)]
mod stall_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::diagnostics::Observation;
    use otto_core::model::{EffectCertainty, Usage};
    use otto_core::provider::{ProviderError, ProviderSettlement, Response, StreamEvent};

    fn records(home: &Path) -> Vec<Value> {
        std::fs::read_dir(home.join(".otto/logs"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
            .flat_map(|entry| {
                std::fs::read_to_string(entry.path())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn new_owner_lock_waits_for_a_brief_cleanup_probe() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("probe.lock");
        let owner = File::create(&path).unwrap();
        let probe = File::open(&path).unwrap();
        fsops::try_lock_exclusive(&probe).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(probe);
        });
        lock_new_owner(&owner).expect("lock acquired after the probe is released");
        release.join().unwrap();
    }

    #[test]
    fn runtimes_have_separate_files_and_bounded_rotation() {
        let home = tempfile::tempdir().unwrap();
        let mut a = Writer::open(home.path(), "test").unwrap();
        let mut b = Writer::open(home.path(), "test").unwrap();
        assert_ne!(a.prefix, b.prefix);
        a.limit = 512;
        for n in 0..30 {
            a.write(json!({"event":"sample","n":n,"padding":"x".repeat(100)}))
                .unwrap();
        }
        b.write(json!({"event":"other_runtime"})).unwrap();
        let names = fsops::read_directory_names(&a.dir, 100).unwrap();
        assert!(names.contains(&format!("{}.jsonl.3", a.prefix)));
        assert!(!names.contains(&format!("{}.jsonl.4", a.prefix)));
        assert_eq!(
            std::fs::read_to_string(
                home.path()
                    .join(".otto/logs")
                    .join(format!("{}.jsonl", b.prefix))
            )
            .unwrap()
            .lines()
            .count(),
            1
        );
        assert_eq!(
            a.file.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn retention_skips_active_families_and_unrelated_files() {
        let home = tempfile::tempdir().unwrap();
        let a = Writer::open(home.path(), "test").unwrap();
        let b = Writer::open(home.path(), "test").unwrap();
        let a_path = home
            .path()
            .join(".otto/logs")
            .join(format!("{}.jsonl", a.prefix));
        let b_path = home
            .path()
            .join(".otto/logs")
            .join(format!("{}.jsonl", b.prefix));
        let unrelated = home.path().join(".otto/logs/notes.txt");
        std::fs::write(&unrelated, "keep").unwrap();
        let future = SystemTime::now() + Duration::from_secs(60);
        cleanup(&b.dir, future, Duration::ZERO);
        assert!(a_path.exists());
        assert!(b_path.exists());
        drop(a);
        // A test that spawns a process at the same time duplicates `a`'s
        // descriptor into the child until `exec` (see
        // `fsops::SESSION_LOCK_SPAWN_GRACE`), so the probe lock in `cleanup`
        // can fail during that period. Production runs `cleanup` again later;
        // the test repeats it for the same grace period.
        let deadline = std::time::Instant::now() + fsops::SESSION_LOCK_SPAWN_GRACE;
        loop {
            cleanup(&b.dir, future, Duration::ZERO);
            if !a_path.exists() || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!a_path.exists());
        assert!(b_path.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn observations_keep_metadata_and_never_stream_or_error_bodies() {
        let home = tempfile::tempdir().unwrap();
        let log = Log::open(home.path(), "test").unwrap();
        let secret = "secret-abc";
        let redactor = Redactor::new(&[secret.into()]);
        let scope = log.scope("session-a", "", redactor, Default::default());
        let child = scope.for_task("t1").unwrap();
        let mut observation = Observation::start(
            Some(&child),
            Kind::Provider,
            "op_a",
            None,
            "chatgpt",
            secret,
        );
        observation.observer().activity(Phase::Sending, 0, None);
        observation
            .observer()
            .activity(Phase::Headers, 0, Some(200));
        observation.observer().activity(Phase::Receiving, 100, None);
        observation.stream(&StreamEvent::TextDelta {
            text: secret.into(),
        });
        log.heartbeat();
        observation.provider_finished(&ProviderSettlement::failed(
            ProviderError::Other(secret.into()),
            1,
            EffectCertainty::Unknown,
        ));
        let before = records(home.path()).len();
        log.heartbeat();
        let rows = records(home.path());
        assert_eq!(rows.len(), before);
        assert!(!serde_json::to_string(&rows).unwrap().contains(secret));
        let heartbeat = rows.iter().find(|r| r["event"] == "heartbeat").unwrap();
        assert_eq!(heartbeat["session_id"], "session-a");
        assert_eq!(heartbeat["task_id"], "t1");
        assert_eq!(heartbeat["operation_id"], "op_a");
        assert_eq!(heartbeat["http_status"], 200);
        assert_eq!(heartbeat["network_bytes"], 100);
        assert!(!heartbeat["last_model_ms"].is_null());
        assert!(rows.iter().any(
            |r| r["event"] == "operation_finished" && r["finish"]["error_class"] == "provider"
        ));
    }

    #[test]
    fn abandonment_and_missing_usage_are_not_reported_as_zero_success() {
        let home = tempfile::tempdir().unwrap();
        let log = Log::open(home.path(), "test").unwrap();
        let scope = log.scope("session", "", Redactor::new(&[]), Default::default());
        {
            let _pending = Observation::start(
                Some(&scope),
                Kind::Tool,
                "op_pending",
                Some("bash"),
                "chatgpt",
                "model",
            );
        }
        for (id, usage, cache_present) in [
            ("op_missing", None, false),
            ("op_zero", Some(Usage::default()), true),
            ("op_cache_unknown", Some(Usage::default()), false),
        ] {
            let mut observation =
                Observation::start(Some(&scope), Kind::Provider, id, None, "chatgpt", "model");
            observation.provider_finished(&ProviderSettlement::succeeded(
                Response {
                    message: otto_core::model::Message {
                        usage,
                        ..Default::default()
                    },
                    cached_input_tokens_present: cache_present,
                    ..Response::default()
                },
                1,
            ));
        }
        let rows = records(home.path());
        assert!(
            rows.iter()
                .any(|r| r["event"] == "operation_abandoned" && r["operation_id"] == "op_pending")
        );
        let missing = rows
            .iter()
            .find(|r| r["event"] == "operation_finished" && r["operation_id"] == "op_missing")
            .unwrap();
        let zero = rows
            .iter()
            .find(|r| r["event"] == "operation_finished" && r["operation_id"] == "op_zero")
            .unwrap();
        assert!(missing["finish"].get("usage").is_none());
        assert_eq!(zero["finish"]["usage"]["input_tokens"], 0);
        assert_eq!(zero["finish"]["cached_input_tokens_present"], true);
        let unknown_cache = rows
            .iter()
            .find(|r| r["event"] == "operation_finished" && r["operation_id"] == "op_cache_unknown")
            .unwrap();
        assert!(unknown_cache["finish"].get("usage").is_some());
        assert_eq!(
            unknown_cache["finish"]["cached_input_tokens_present"],
            false
        );
        assert!(
            missing["finish"]
                .get("cached_input_tokens_present")
                .is_none()
        );
    }

    #[test]
    fn log_directory_symlink_is_rejected() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".otto")).unwrap();
        std::os::unix::fs::symlink(outside.path(), home.path().join(".otto/logs")).unwrap();
        assert!(Writer::open(home.path(), "test").is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
