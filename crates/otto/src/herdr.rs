//! Reports this session's lifecycle to a Herdr pane.
//!
//! Herdr is a terminal workspace manager for coding agents. Every bundled
//! integration is the same protocol on the agent side: the agent reports its
//! own state over the socket Herdr injects through the environment, and Herdr
//! treats that report as the authority for the pane's agent. Otto is not a
//! bundled agent, so this module *is* Otto's integration:
//!
//! - `working` while a turn runs,
//! - `blocked` while a human decision is pending, with the request title as the
//!   message, so `herdr agent wait otto --until blocked` and the sidebar
//!   notify the user who is working elsewhere,
//! - `idle` once the session waits for input,
//! - the session reference (`agent_session_id`, `agent_session_path`,
//!   `resume_argv`) Herdr shows and restores the session with.
//!
//! Everything here is inert unless Herdr started the process: without
//! `HERDR_ENV=1`, `HERDR_PANE_ID` and `HERDR_SOCKET_PATH` there is no socket,
//! no task and no work. Reporting is best-effort in the other direction too:
//! a missing socket or a refused connection is dropped, never surfaced, so
//! Otto behaves identically with and without Herdr.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use otto_core::agent::Event;
use otto_core::approval::State as ApprovalState;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

/// The stable integration id every report carries.
pub const SOURCE: &str = "custom:otto";
/// The label Herdr shows for the agent occupying the pane.
pub const AGENT: &str = "otto";

/// How long one report waits for Herdr before it is dropped.
const REPORT_TIMEOUT: Duration = Duration::from_millis(500);
/// A logical turn may span several agent runs (follow-up steps, wake turns).
/// An `idle` waits this long for the next one, so the sidebar does not flap.
const IDLE_DEBOUNCE: Duration = Duration::from_millis(300);

/// What Otto tells Herdr about the session, for display and restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub id: String,
    pub path: String,
}

impl SessionRef {
    /// The command Herdr runs to bring this session back after a restart. The
    /// first element must be a plain command name; the path is the session
    /// file `otto --resume` reopens.
    pub fn resume_argv(&self) -> Vec<String> {
        vec!["otto".to_owned(), "--resume".to_owned(), self.path.clone()]
    }

    fn is_empty(&self) -> bool {
        self.id.is_empty() || self.path.is_empty()
    }
}

/// The pane Herdr started this process in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub pane_id: String,
    pub socket_path: PathBuf,
}

impl Config {
    /// The pane this process runs in, or `None` outside Herdr. All three
    /// variables come from Herdr itself, so a stray environment cannot enable
    /// reporting halfway.
    pub fn from_environment(environment: &HashMap<String, String>) -> Option<Self> {
        if environment.get("HERDR_ENV").map(String::as_str) != Some("1") {
            return None;
        }
        let pane_id = environment.get("HERDR_PANE_ID")?.trim();
        let socket = environment.get("HERDR_SOCKET_PATH")?.trim();
        if pane_id.is_empty() || socket.is_empty() {
            return None;
        }
        Some(Self {
            pane_id: pane_id.to_owned(),
            socket_path: PathBuf::from(socket),
        })
    }
}

/// The lifecycle states Otto reports. `done` is derived by Herdr from an
/// unseen `idle` and is never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Working,
    Blocked,
    Idle,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
        }
    }
}

enum Command {
    State {
        state: State,
        message: Option<String>,
        seq: u64,
    },
    Session {
        session: SessionRef,
        seq: u64,
    },
    Release(oneshot::Sender<()>),
}

/// Reports state and the session reference to one Herdr pane.
///
/// Reporting never blocks the caller: [`Reporter::observe`] decides what to
/// send and hands it to a background task that owns the socket. The task is
/// also where the `idle` debounce lives, because only it can time.
pub struct Reporter {
    tx: mpsc::UnboundedSender<Command>,
    seq: AtomicU64,
    last_state: Mutex<Option<(State, Option<String>)>>,
    last_session: Mutex<Option<SessionRef>>,
}

impl std::fmt::Debug for Reporter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("herdr::Reporter").finish()
    }
}

impl Reporter {
    pub fn start(config: Config) -> Arc<Self> {
        Self::start_with_debounce(config, IDLE_DEBOUNCE)
    }

    /// [`Reporter::start`] with an explicit idle window, so tests do not wait.
    pub fn start_with_debounce(config: Config, debounce: Duration) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let reporter = Arc::new(Self {
            tx,
            seq: AtomicU64::new(now_nanos()),
            last_state: Mutex::new(None),
            last_session: Mutex::new(None),
        });
        tokio::spawn(report_loop(config, rx, debounce));
        reporter
    }

    /// Turns one agent event into a report. Called for every event of a run,
    /// so it only inspects the three that carry lifecycle meaning.
    pub fn observe(&self, event: &Event, session: &SessionRef) {
        match event {
            Event::AgentStarted => {
                self.report_session(session);
                self.report_state(State::Working, None);
            }
            Event::AgentFinished => self.report_state(State::Idle, None),
            Event::ApprovalChanged { request } => match request.state {
                ApprovalState::Pending => {
                    let message = request.title.trim().to_owned();
                    self.report_state(State::Blocked, Some(message));
                }
                // The decision reopens the turn the request interrupted, and
                // the next `AgentStarted` follows immediately. Leaving
                // `blocked` is still worth a report, but only from `blocked`:
                // an approval decided after the turn ended must not revive
                // `working` and strand the pane there.
                _ => {
                    if self.reported_blocked() {
                        self.report_state(State::Working, None);
                    }
                }
            },
            _ => {}
        }
    }

    /// Tells Herdr this pane no longer holds an agent, so it keeps no stale
    /// row. Called once on the way out.
    pub async fn release(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Command::Release(done)).is_err() {
            return;
        }
        let _ = tokio::time::timeout(REPORT_TIMEOUT, wait).await;
    }

    fn report_state(&self, state: State, message: Option<String>) {
        let next = (state, message);
        if !state_changed(&self.last_state, &next) {
            return;
        }
        let seq = self.next_seq();
        let _ = self.tx.send(Command::State {
            state,
            message: next.1,
            seq,
        });
    }

    fn report_session(&self, session: &SessionRef) {
        if session.is_empty() {
            return;
        }
        {
            let mut last = lock(&self.last_session);
            if last.as_ref() == Some(session) {
                return;
            }
            *last = Some(session.clone());
        }
        let seq = self.next_seq();
        let _ = self.tx.send(Command::Session {
            session: session.clone(),
            seq,
        });
    }

    /// Whether the last state reported was `blocked`.
    fn reported_blocked(&self) -> bool {
        let last = lock(&self.last_state);
        matches!(last.as_ref(), Some((State::Blocked, _)))
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }
}

/// Whether `next` differs from the last state sent, recording it when it does.
fn state_changed(
    last: &Mutex<Option<(State, Option<String>)>>,
    next: &(State, Option<String>),
) -> bool {
    let mut last = lock(last);
    if last.as_ref() == Some(next) {
        return false;
    }
    *last = Some(next.clone());
    true
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(1)
}

/// Decorates an event sink with the report, exactly like
/// [`crate::app::auto_reflection`] does for its observation. A frontend that
/// runs a turn therefore reports without knowing about Herdr.
pub fn observing<'a>(
    emit: otto_core::agent::EventSink<'a>,
    reporter: Option<&'a Reporter>,
    session: &'a SessionRef,
) -> impl FnMut(Event) + Send + use<'a> {
    move |event| {
        if let Some(reporter) = reporter {
            reporter.observe(&event, session);
        }
        emit(event);
    }
}

/// Owns the socket: one connection per report, dropped on any failure.
async fn report_loop(config: Config, mut rx: mpsc::UnboundedReceiver<Command>, debounce: Duration) {
    let mut idle: Option<(u64, tokio::time::Instant)> = None;
    loop {
        let wait_for_idle = async {
            match idle {
                Some((_, deadline)) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            command = rx.recv() => match command {
                None => break,
                Some(Command::State { state: State::Idle, seq, .. }) => {
                    // A later `working` cancels this before it is sent.
                    idle = Some((seq, tokio::time::Instant::now() + debounce));
                }
                Some(Command::State { state, message, seq }) => {
                    idle = None;
                    let params = report_agent_params(&config.pane_id, state.as_str(), message.as_deref(), seq);
                    send(&config.socket_path, "pane.report_agent", params).await;
                }
                Some(Command::Session { session, seq }) => {
                    let params = report_agent_session_params(&config.pane_id, &session, seq);
                    send(&config.socket_path, "pane.report_agent_session", params).await;
                }
                Some(Command::Release(done)) => {
                    let params = json!({
                        "pane_id": config.pane_id,
                        "source": SOURCE,
                        "agent": AGENT,
                    });
                    send(&config.socket_path, "pane.release_agent", params).await;
                    let _ = done.send(());
                    break;
                }
            },
            _ = wait_for_idle => {
                if let Some((seq, _)) = idle.take() {
                    let params = report_agent_params(&config.pane_id, State::Idle.as_str(), None, seq);
                    send(&config.socket_path, "pane.report_agent", params).await;
                }
            }
        }
    }
}

fn report_agent_params(
    pane_id: &str,
    state: &str,
    message: Option<&str>,
    seq: u64,
) -> serde_json::Value {
    let mut params = json!({
        "pane_id": pane_id,
        "source": SOURCE,
        "agent": AGENT,
        "state": state,
        "seq": seq,
    });
    if let Some(message) = message.filter(|text| !text.is_empty()) {
        params["message"] = serde_json::Value::String(message.to_owned());
    }
    params
}

fn report_agent_session_params(pane_id: &str, session: &SessionRef, seq: u64) -> serde_json::Value {
    json!({
        "pane_id": pane_id,
        "source": SOURCE,
        "agent": AGENT,
        "seq": seq,
        "agent_session_id": session.id,
        "agent_session_path": session.path,
        "resume_argv": session.resume_argv(),
    })
}

/// Writes one newline-delimited request and forgets the outcome, including a
/// socket that does not exist.
async fn send(socket_path: &std::path::Path, method: &str, params: serde_json::Value) {
    let request = json!({
        "id": format!("{SOURCE}:{}", now_nanos()),
        "method": method,
        "params": params,
    });
    let mut line = match serde_json::to_vec(&request) {
        Ok(line) => line,
        Err(_) => return,
    };
    line.push(b'\n');
    let write = async {
        let mut stream = UnixStream::connect(socket_path).await?;
        stream.write_all(&line).await?;
        stream.flush().await
    };
    let _ = tokio::time::timeout(REPORT_TIMEOUT, write).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn config_requires_the_whole_herdr_environment() {
        let complete = environment(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
        ]);
        assert_eq!(
            Config::from_environment(&complete),
            Some(Config {
                pane_id: "w1:p1".to_owned(),
                socket_path: PathBuf::from("/tmp/herdr.sock"),
            })
        );

        for missing in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"] {
            let mut pairs = complete.clone();
            pairs.remove(missing);
            assert!(
                Config::from_environment(&pairs).is_none(),
                "missing {missing} must not enable reporting"
            );
        }

        let mut outside = complete.clone();
        outside.insert("HERDR_ENV".to_owned(), "0".to_owned());
        assert!(Config::from_environment(&outside).is_none());

        let mut blank = complete;
        blank.insert("HERDR_PANE_ID".to_owned(), "   ".to_owned());
        assert!(Config::from_environment(&blank).is_none());
    }

    #[test]
    fn state_names_match_the_herdr_schema() {
        assert_eq!(State::Working.as_str(), "working");
        assert_eq!(State::Blocked.as_str(), "blocked");
        assert_eq!(State::Idle.as_str(), "idle");
    }

    #[test]
    fn unchanged_state_is_not_reported_twice() {
        let last = Mutex::new(None);
        let working = (State::Working, None);
        assert!(state_changed(&last, &working));
        assert!(!state_changed(&last, &working));
        assert!(state_changed(
            &last,
            &(State::Blocked, Some("Run rm -rf".into()))
        ));
        assert!(state_changed(
            &last,
            &(State::Blocked, Some("Run make".into()))
        ));
        assert!(state_changed(&last, &working));
    }

    #[test]
    fn agent_report_matches_the_socket_schema() {
        let params = report_agent_params("w1:p1", "blocked", Some("Run rm -rf build"), 7);
        assert_eq!(params["pane_id"], "w1:p1");
        assert_eq!(params["source"], SOURCE);
        assert_eq!(params["agent"], AGENT);
        assert_eq!(params["state"], "blocked");
        assert_eq!(params["message"], "Run rm -rf build");
        assert_eq!(params["seq"], 7);

        let quiet = report_agent_params("w1:p1", "idle", None, 8);
        assert!(quiet.get("message").is_none());
        assert_eq!(quiet["state"], "idle");
    }

    #[test]
    fn session_report_carries_the_resume_command() {
        let session = SessionRef {
            id: "s-1".to_owned(),
            path: "/home/me/.otto/sessions/s-1.jsonl".to_owned(),
        };
        let params = report_agent_session_params("w1:p1", &session, 9);
        assert_eq!(params["pane_id"], "w1:p1");
        assert_eq!(params["agent_session_id"], "s-1");
        assert_eq!(params["agent_session_path"], session.path);
        assert_eq!(
            params["resume_argv"],
            json!(["otto", "--resume", "/home/me/.otto/sessions/s-1.jsonl"])
        );
    }

    #[test]
    fn a_session_without_ids_is_not_reported() {
        let empty = SessionRef {
            id: String::new(),
            path: "/tmp/s.jsonl".to_owned(),
        };
        assert!(empty.is_empty());
        assert!(
            !SessionRef {
                id: "s".into(),
                path: "/tmp/s.jsonl".into()
            }
            .is_empty()
        );
    }

    #[test]
    fn the_decorator_forwards_every_event() {
        let session = SessionRef {
            id: "s-1".to_owned(),
            path: "/tmp/s-1.jsonl".to_owned(),
        };
        let mut seen: Vec<&'static str> = Vec::new();
        {
            let mut record = |event: Event| seen.push(event.name());
            let mut sink = observing(
                &mut record as &mut (dyn FnMut(Event) + Send),
                None,
                &session,
            );
            sink(Event::AgentStarted);
            sink(Event::AgentFinished);
        }
        assert_eq!(seen, ["agent_started", "agent_finished"]);
    }
}
