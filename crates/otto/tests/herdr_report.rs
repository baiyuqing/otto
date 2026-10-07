//! Herdr reporting: the socket protocol, the state mapping and the idle
//! debounce.
//!
//! A `UnixListener` on a temporary path stands in for the Herdr server, so
//! these tests need no Herdr, no provider and no network. The request shapes
//! asserted here are the ones `herdr api schema` publishes for
//! `pane.report_agent`, `pane.report_agent_session` and `pane.release_agent`.

use std::path::PathBuf;
use std::time::Duration;

use otto::herdr::{Config, Reporter, SessionRef};
use otto_core::agent::Event;
use otto_core::approval::{
    ApplicationState, Authority, Choice, Kind, Request, Scope, State as ApprovalState, Subject,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// A stand-in for the Herdr socket: accepts the same connections the real
/// server does and hands every parsed request to the test.
struct FakeHerdr {
    socket_path: PathBuf,
    requests: mpsc::UnboundedReceiver<Value>,
    /// Held so the socket path survives for the test's lifetime.
    _dir: tempfile::TempDir,
}

fn fake_herdr() -> FakeHerdr {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind the fake Herdr socket");
    let (tx, requests) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(request) = serde_json::from_str::<Value>(&line) {
                        let _ = tx.send(request);
                    }
                }
            });
        }
    });
    FakeHerdr {
        socket_path,
        requests,
        _dir: dir,
    }
}

impl FakeHerdr {
    fn config(&self) -> Config {
        Config {
            pane_id: "w1:p1".to_owned(),
            socket_path: self.socket_path.clone(),
        }
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("a report within 5s")
            .expect("the fake Herdr stays open")
    }
}

fn session() -> SessionRef {
    SessionRef {
        id: "s-1".to_owned(),
        path: "/tmp/otto/s-1.jsonl".to_owned(),
    }
}

fn request(state: ApprovalState) -> Request {
    Request {
        options: vec![Choice {
            id: "approve".to_owned(),
            label: "Approve".to_owned(),
            scope: "once".to_owned(),
        }],
        schema_version: 1,
        id: "req-1".to_owned(),
        revision: 1,
        kind: Kind::Authorization,
        authority: Authority::Boundary,
        scope: Scope {
            workspace: "/tmp/otto".to_owned(),
            session_id: "s-1".to_owned(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: Subject {
            tool_call_id: String::new(),
            source: "bash".to_owned(),
            id: "call-1".to_owned(),
            version: "1".to_owned(),
            details: String::new(),
        },
        title: "Run rm -rf build".to_owned(),
        reason: String::new(),
        persistent: false,
        blocking: true,
        created_at: 0,
        expires_at: None,
        state,
        decision: None,
        application: ApplicationState::NotStarted,
        application_error: String::new(),
    }
}

#[tokio::test]
async fn reports_the_turn_lifecycle_and_the_session_reference() {
    let mut herdr = fake_herdr();
    let reporter = Reporter::start_with_debounce(herdr.config(), Duration::from_millis(50));
    let session = session();

    reporter.observe(&Event::AgentStarted, &session);

    // The state goes first: Herdr accepts the session reference (and its
    // `resume_argv`) only from a reporter that already holds the pane.
    let working = herdr.next().await;
    assert_eq!(working["method"], "pane.report_agent");
    assert_eq!(working["params"]["state"], "working");

    let session_report = herdr.next().await;
    assert_eq!(session_report["method"], "pane.report_agent_session");
    assert_eq!(session_report["params"]["pane_id"], "w1:p1");
    assert_eq!(session_report["params"]["source"], "custom:otto");
    assert_eq!(session_report["params"]["agent"], "otto");
    assert_eq!(session_report["params"]["agent_session_id"], "s-1");
    assert_eq!(
        session_report["params"]["agent_session_path"],
        "/tmp/otto/s-1.jsonl"
    );
    assert_eq!(
        session_report["params"]["resume_argv"],
        json!(["otto", "--resume", "/tmp/otto/s-1.jsonl"])
    );

    reporter.observe(
        &Event::ApprovalChanged {
            request: Box::new(request(ApprovalState::Pending)),
        },
        &session,
    );
    let blocked = herdr.next().await;
    assert_eq!(blocked["params"]["state"], "blocked");
    assert_eq!(blocked["params"]["message"], "Run rm -rf build");

    reporter.observe(
        &Event::ApprovalChanged {
            request: Box::new(request(ApprovalState::Approved)),
        },
        &session,
    );
    let resumed = herdr.next().await;
    assert_eq!(resumed["params"]["state"], "working");

    reporter.observe(&Event::AgentFinished, &session);
    let idle = herdr.next().await;
    assert_eq!(idle["params"]["state"], "idle");
    assert!(
        idle["params"].get("message").is_none(),
        "an idle report carries no message"
    );

    reporter.release().await;
    let released = herdr.next().await;
    assert_eq!(released["method"], "pane.release_agent");
    assert_eq!(released["params"]["agent"], "otto");
}

/// A Herdr pane is nobody until the agent inside it reports, so the pane is
/// claimed when the frontend starts, not when the first turn begins.
#[tokio::test]
async fn the_pane_is_announced_before_the_first_turn() {
    let mut herdr = fake_herdr();
    // A long debounce: the announcement must not wait for it.
    let reporter = Reporter::start_with_debounce(herdr.config(), Duration::from_millis(300));
    let session = session();

    reporter.ready(&session);

    let announced = herdr.next().await;
    assert_eq!(announced["method"], "pane.report_agent");
    assert_eq!(announced["params"]["pane_id"], "w1:p1");
    assert_eq!(announced["params"]["source"], "custom:otto");
    assert_eq!(announced["params"]["agent"], "otto");
    assert_eq!(announced["params"]["state"], "idle");

    let reference = herdr.next().await;
    assert_eq!(reference["method"], "pane.report_agent_session");
    assert_eq!(reference["params"]["agent_session_id"], "s-1");
    assert_eq!(
        reference["params"]["resume_argv"],
        json!(["otto", "--resume", "/tmp/otto/s-1.jsonl"])
    );

    // The first turn changes the state and repeats neither report.
    reporter.observe(&Event::AgentStarted, &session);
    let working = herdr.next().await;
    assert_eq!(working["method"], "pane.report_agent");
    assert_eq!(working["params"]["state"], "working");
    let quiet = tokio::time::timeout(Duration::from_millis(200), herdr.requests.recv()).await;
    assert!(
        quiet.is_err(),
        "the announced state and the session reference are sent once"
    );
}

/// The session can be missing when the frontend starts; the pane is still
/// claimed, and no session reference is invented.
#[tokio::test]
async fn announcing_without_a_session_claims_the_pane() {
    let mut herdr = fake_herdr();
    let reporter = Reporter::start_with_debounce(herdr.config(), Duration::from_millis(50));
    let empty = SessionRef {
        id: String::new(),
        path: String::new(),
    };

    reporter.ready(&empty);

    assert_eq!(herdr.next().await["params"]["state"], "idle");
    let quiet = tokio::time::timeout(Duration::from_millis(200), herdr.requests.recv()).await;
    assert!(quiet.is_err(), "no session reference without a session");
}

#[tokio::test]
async fn a_later_step_cancels_the_idle_report() {
    let mut herdr = fake_herdr();
    let reporter = Reporter::start_with_debounce(herdr.config(), Duration::from_millis(300));
    let session = session();

    reporter.observe(&Event::AgentStarted, &session);
    assert_eq!(herdr.next().await["params"]["state"], "working");
    assert_eq!(herdr.next().await["method"], "pane.report_agent_session");

    // A follow-up step of the same logical turn: the finished run must not
    // show up as `idle` in Herdr's sidebar.
    reporter.observe(&Event::AgentFinished, &session);
    tokio::time::sleep(Duration::from_millis(50)).await;
    reporter.observe(&Event::AgentStarted, &session);

    let next = herdr.next().await;
    assert_eq!(
        next["params"]["state"], "working",
        "the next report is the new step, not the cancelled idle"
    );
    let quiet = tokio::time::timeout(Duration::from_millis(400), herdr.requests.recv()).await;
    assert!(quiet.is_err(), "the cancelled idle report never arrives");
}

#[tokio::test]
async fn an_approval_decided_after_the_turn_does_not_revive_working() {
    let mut herdr = fake_herdr();
    let reporter = Reporter::start_with_debounce(herdr.config(), Duration::from_millis(50));
    let session = session();

    reporter.observe(&Event::AgentStarted, &session);
    assert_eq!(herdr.next().await["params"]["state"], "working");
    assert_eq!(herdr.next().await["method"], "pane.report_agent_session");

    // The turn ended and the decision arrives afterwards: the pane must settle
    // on `idle` rather than be stranded in `working`.
    reporter.observe(&Event::AgentFinished, &session);
    reporter.observe(
        &Event::ApprovalChanged {
            request: Box::new(request(ApprovalState::Approved)),
        },
        &session,
    );

    let next = herdr.next().await;
    assert_eq!(
        next["params"]["state"], "idle",
        "a decision after the turn must not report working"
    );
}

#[tokio::test]
async fn an_unreachable_socket_changes_nothing() {
    let config = Config {
        pane_id: "w1:p1".to_owned(),
        socket_path: PathBuf::from("/nonexistent/herdr.sock"),
    };
    let reporter = Reporter::start_with_debounce(config, Duration::from_millis(10));
    let session = session();

    reporter.ready(&session);
    reporter.observe(&Event::AgentStarted, &session);
    reporter.observe(&Event::AgentFinished, &session);
    tokio::time::timeout(Duration::from_secs(2), reporter.release())
        .await
        .expect("release returns even when Herdr is gone");
}
