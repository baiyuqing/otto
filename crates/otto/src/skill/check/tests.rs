use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;

// ---------------------------------------------------------------------------
// HTTP stub, mirroring crates/otto/src/provider/openaicompat.rs's TestServer.
// ---------------------------------------------------------------------------

struct TestServer {
    base_url: String,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn spawn_server<H>(handler: H) -> TestServer
where
    H: Fn(&str, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("listener has an address")
    );
    let handler = Arc::new(handler);
    let accept = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut stream).await else {
                    return;
                };
                let response = handler(&head, &body);
                if !response.is_empty() {
                    let _ = stream.write_all(&response).await;
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    TestServer { base_url, accept }
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut raw: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&buffer[..read]);
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let length = head
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:").map(str::to_string))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = raw[head_end..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Some((head, body))
}

fn http_response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A response with every question answered `procedural` and a `score` at
/// the top level, with a high `noul`/confidence, i.e. nothing flagged.
fn clean_answers_body() -> String {
    serde_json::json!({
        "model": "jev-1.13.0",
        "answers": {
            "procedural": {"type": "choice", "choice": "procedural", "confidence": 0.9},
            "self_contained": {"type": "noul", "noul": 0.9},
            "output_usable": {"type": "noul", "noul": 0.9},
            "body_matches_contract": {"type": "score", "score": 2.0, "confidence": 0.9},
        },
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

#[test]
fn context_reference_fires_on_each_phrase() {
    for phrase in CONTEXT_REFERENCE_PATTERNS {
        let body = format!("Continue the task {phrase} and finish it.");
        assert!(context_reference_fires(&body), "{phrase}");
    }
}

#[test]
fn context_reference_is_case_insensitive() {
    assert!(context_reference_fires("As Discussed, proceed."));
}

#[test]
fn context_reference_does_not_fire_on_self_contained_body() {
    assert!(!context_reference_fires(
        "Read the file named in `input.path` and summarize it."
    ));
}

#[test]
fn vague_output_fires_on_stock_phrases() {
    for phrase in VAGUE_OUTPUT_PHRASES {
        assert!(vague_output_fires(phrase), "{phrase}");
        assert!(vague_output_fires(&phrase.to_uppercase()), "{phrase}");
    }
}

#[test]
fn vague_output_fires_under_four_words() {
    assert!(vague_output_fires("three word text"));
}

#[test]
fn vague_output_does_not_fire_at_four_words() {
    assert!(!vague_output_fires("exactly four words here"));
}

#[test]
fn vague_output_does_not_fire_on_specific_output() {
    assert!(!vague_output_fires(
        "A JSON object with `path` and `line` fields for each match."
    ));
}

#[test]
fn vague_output_ignores_trailing_punctuation() {
    assert!(vague_output_fires("The result."));
}

#[test]
fn questions_sha256_is_stable_and_hex() {
    let first = questions_sha256();
    let second = questions_sha256();
    assert_eq!(first, second);
    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn content_sha256_differs_by_content() {
    assert_ne!(content_sha256(b"a"), content_sha256(b"b"));
    assert_eq!(content_sha256(b"a"), content_sha256(b"a"));
}

// ---------------------------------------------------------------------------
// TypeSafeClient
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_sends_bearer_auth_and_all_four_questions() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = Arc::clone(&seen);
    let server = spawn_server(move |head, body| {
        recorded.lock().unwrap().push(head.to_string());
        recorded
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(body).into_owned());
        http_response(200, &clean_answers_body())
    })
    .await;
    let client = TypeSafeClient::new(server.base_url.clone(), "secret-key");

    let answers = client.check("SKILL.md text").await.expect("check succeeds");

    assert_eq!(answers.model, "jev-1.13.0");
    assert_eq!(answers.answers.len(), 4);
    let recorded = seen.lock().unwrap();
    let head = &recorded[0];
    let body = &recorded[1];
    assert!(head.contains("POST /v1/systemone"), "{head}");
    assert!(head.contains("Bearer secret-key"), "{head}");
    assert!(!head.contains("secret-key\n\n"), "leaked past header");
    let parsed: serde_json::Value = serde_json::from_str(body).expect("json body");
    assert_eq!(parsed["model"], "jev-latest");
    let questions = parsed["questions"].as_object().expect("questions object");
    assert_eq!(questions.len(), 4);
    assert!(
        questions["procedural"].get("id").is_none(),
        "no id field inside a question"
    );
    assert_eq!(questions["procedural"]["type"], "choice");
    assert_eq!(
        questions["procedural"]["criteria"]["procedural"],
        "ordered steps that take the declared input to the declared output."
    );
    assert_eq!(questions["self_contained"]["type"], "noul");
    assert_eq!(
        questions["self_contained"]["criteria"]["true"],
        "everything the steps need is named in `input` or in the skill package."
    );
    assert_eq!(questions["body_matches_contract"]["type"], "score");
    assert_eq!(
        questions["body_matches_contract"]["criteria"],
        serde_json::json!(["does not match", "partly matches", "matches"])
    );
}

#[tokio::test]
async fn client_retries_429_then_succeeds_and_writes_one_row() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            http_response(429, "rate limited")
        } else {
            http_response(200, &clean_answers_body())
        }
    })
    .await;
    let client =
        TypeSafeClient::new(server.base_url.clone(), "key").with_backoff(vec![Duration::ZERO]);

    let answers = client.check("state").await.expect("retry then succeed");

    assert_eq!(answers.answers.len(), 4);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn client_401_is_not_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        http_response(401, "unauthorized")
    })
    .await;
    let client = TypeSafeClient::new(server.base_url.clone(), "key").with_backoff(vec![]);

    let error = client.check("state").await.expect_err("401 is an error");

    assert_eq!(error, CheckError::Status(401));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn client_missing_answer_is_invalid_response() {
    let server = spawn_server(|_head, _body| {
        let body = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "procedural": {"type": "choice", "choice": "procedural", "confidence": 0.9},
            },
        })
        .to_string();
        http_response(200, &body)
    })
    .await;
    let client = TypeSafeClient::new(server.base_url.clone(), "key");

    let error = client.check("state").await.expect_err("missing answers");

    assert_eq!(error, CheckError::InvalidResponse);
}

#[test]
fn check_error_reasons_never_mention_the_key() {
    assert_eq!(CheckError::Status(429).reason(), "429");
    assert_eq!(CheckError::Timeout.reason(), "timeout");
    assert_eq!(CheckError::Transport.reason(), "transport error");
    assert_eq!(CheckError::InvalidResponse.reason(), "invalid response");
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

fn sample_row(result: &ResultJson) -> NewRow<'_> {
    NewRow {
        content_sha256: "hash-a",
        questions_sha256: "qhash",
        skill: "reviewer",
        path: "/skills/reviewer/SKILL.md",
        source: "rules",
        model: "",
        checked_at: "2026-09-25T00:00:00Z",
        result,
    }
}

#[test]
fn store_append_then_latest_round_trips() {
    let store = Store::open_in_memory().expect("open");
    let result = ResultJson::Rules {
        rules: vec!["vague_output".to_string()],
    };
    store.append(&sample_row(&result)).expect("append");

    let row = store
        .latest("hash-a", "qhash")
        .expect("query")
        .expect("row present");

    assert_eq!(row.source, "rules");
    assert_eq!(row.result, result);
}

#[test]
fn store_latest_is_none_for_unknown_key() {
    let store = Store::open_in_memory().expect("open");
    assert_eq!(store.latest("nope", "nope").expect("query"), None);
}

#[test]
fn store_keeps_old_rows_when_a_new_key_is_appended() {
    let store = Store::open_in_memory().expect("open");
    let first = ResultJson::Rules {
        rules: vec!["context_reference".to_string()],
    };
    store.append(&sample_row(&first)).expect("append first");

    let mut second_row = sample_row(&first);
    second_row.content_sha256 = "hash-b";
    let second = ResultJson::Rules { rules: vec![] };
    second_row.result = &second;
    store.append(&second_row).expect("append second");

    assert_eq!(
        store.latest("hash-a", "qhash").expect("query"),
        Some(Row {
            source: "rules".to_string(),
            model: String::new(),
            checked_at: "2026-09-25T00:00:00Z".to_string(),
            result: first,
        })
    );
    assert!(store.latest("hash-b", "qhash").expect("query").is_some());
}

#[test]
fn store_open_rejects_an_unrecognized_schema_version() {
    let connection = Connection::open_in_memory().expect("open");
    connection.execute_batch(SCHEMA).expect("create schema");
    connection
        .pragma_update(None, "user_version", 999)
        .expect("set version");

    let error = Store::initialize(connection).expect_err("unknown version");

    assert_eq!(error, StoreError::UnknownVersion);
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// Builds an `answers` map (as stored and read back) from a JSON object
/// literal, one entry per question id.
fn answers_map(value: serde_json::Value) -> AnswerMap {
    value.as_object().expect("answers object").clone()
}

#[test]
fn hint_flags_non_procedural_choice() {
    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "procedural": {"type": "choice", "choice": "mixed", "confidence": 0.9},
        })),
    };
    assert_eq!(hint_for("procedural", &result), Hint::Flagged);
}

#[test]
fn hint_ok_for_procedural_choice() {
    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "procedural": {"type": "choice", "choice": "procedural", "confidence": 0.9},
        })),
    };
    assert_eq!(hint_for("procedural", &result), Hint::Ok);
}

#[test]
fn hint_flags_low_noul() {
    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "self_contained": {"type": "noul", "noul": 0.2},
        })),
    };
    assert_eq!(hint_for("self_contained", &result), Hint::Flagged);
}

#[test]
fn hint_ok_for_high_noul() {
    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "output_usable": {"type": "noul", "noul": 0.8},
        })),
    };
    assert_eq!(hint_for("output_usable", &result), Hint::Ok);
}

#[test]
fn hint_flags_score_below_the_threshold() {
    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "body_matches_contract": {"type": "score", "score": 1.0, "confidence": 0.9},
        })),
    };
    assert_eq!(hint_for("body_matches_contract", &result), Hint::Flagged);
}

#[test]
fn hint_ok_for_score_at_or_above_the_threshold() {
    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "body_matches_contract": {"type": "score", "score": 1.5, "confidence": 0.9},
        })),
    };
    assert_eq!(hint_for("body_matches_contract", &result), Hint::Ok);
}

#[test]
fn hint_is_uncertain_below_confidence_threshold() {
    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "procedural": {"type": "choice", "choice": "procedural", "confidence": 0.4},
        })),
    };
    assert_eq!(hint_for("procedural", &result), Hint::OkUncertain);

    let result = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "procedural": {"type": "choice", "choice": "mixed", "confidence": 0.4},
        })),
    };
    assert_eq!(hint_for("procedural", &result), Hint::FlaggedUncertain);
}

#[test]
fn hint_not_asked_when_rules_answered_a_different_question() {
    let result = ResultJson::Rules {
        rules: vec!["context_reference".to_string()],
    };
    assert_eq!(hint_for("procedural", &result), Hint::NotAsked);
    assert_eq!(hint_for("self_contained", &result), Hint::Flagged);
}

// ---------------------------------------------------------------------------
// Checker
// ---------------------------------------------------------------------------

fn write_skill(directory: &Path, frontmatter: &str) {
    std::fs::create_dir_all(directory).expect("mkdir");
    std::fs::write(directory.join("SKILL.md"), frontmatter).expect("write SKILL.md");
}

#[tokio::test]
async fn run_writes_a_rules_row_and_never_contacts_typesafe() {
    let server = spawn_server(|_head, _body| panic!("TypeSafe must not be contacted")).await;
    let checker = Arc::new(Checker::open_in_memory(&server.base_url, "key"));
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path\noutput: the result\n---\nBody.\n",
    );

    checker
        .run(
            "reviewer".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("SKILL.md"),
        )
        .await;

    let hash = content_sha256(&std::fs::read(dir.path().join("SKILL.md")).unwrap());
    let row = checker
        .store
        .latest(&hash, &questions_sha256())
        .expect("query")
        .expect("row written");
    assert_eq!(row.source, "rules");
}

#[tokio::test]
async fn run_calls_typesafe_and_writes_an_answers_row_when_no_rule_fires() {
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        http_response(200, &clean_answers_body())
    })
    .await;
    let checker = Arc::new(Checker::open_in_memory(&server.base_url, "key"));
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path to review\noutput: a JSON report with pass/fail per file\n---\nRead `input.path` and produce the report.\n",
    );

    checker
        .run(
            "reviewer".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("SKILL.md"),
        )
        .await;

    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let hash = content_sha256(&std::fs::read(dir.path().join("SKILL.md")).unwrap());
    let row = checker
        .store
        .latest(&hash, &questions_sha256())
        .expect("query")
        .expect("row written");
    assert_eq!(row.source, "typesafe");
}

#[tokio::test]
async fn run_writes_nothing_and_records_the_failure_reason_on_401() {
    let server = spawn_server(|_head, _body| http_response(401, "unauthorized")).await;
    let checker = Arc::new(Checker::open_in_memory(&server.base_url, "key"));
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path to review\noutput: a JSON report with pass/fail per file\n---\nRead `input.path` and produce the report.\n",
    );

    checker
        .run(
            "reviewer".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("SKILL.md"),
        )
        .await;

    let hash = content_sha256(&std::fs::read(dir.path().join("SKILL.md")).unwrap());
    assert_eq!(
        checker
            .store
            .latest(&hash, &questions_sha256())
            .expect("query"),
        None
    );
    assert_eq!(
        checker.failures.lock().unwrap().get("reviewer"),
        Some(&"401".to_string())
    );
}

#[tokio::test]
async fn concurrent_runs_for_the_same_key_send_one_request() {
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = spawn_server(move |_head, _body| {
        counter.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        http_response(200, &clean_answers_body())
    })
    .await;
    let checker = Arc::new(Checker::open_in_memory(&server.base_url, "key"));
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path to review\noutput: a JSON report with pass/fail per file\n---\nRead `input.path` and produce the report.\n",
    );

    let a = checker.run(
        "reviewer".to_string(),
        dir.path().to_path_buf(),
        dir.path().join("SKILL.md"),
    );
    let b = checker.run(
        "reviewer".to_string(),
        dir.path().to_path_buf(),
        dir.path().join("SKILL.md"),
    );
    tokio::join!(a, b);

    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_does_nothing_when_the_key_already_has_a_row() {
    let server = spawn_server(|_head, _body| panic!("TypeSafe must not be contacted")).await;
    let checker = Arc::new(Checker::open_in_memory(&server.base_url, "key"));
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path to review\noutput: a JSON report with pass/fail per file\n---\nRead `input.path` and produce the report.\n",
    );
    let hash = content_sha256(&std::fs::read(dir.path().join("SKILL.md")).unwrap());
    let question_hash = questions_sha256();
    let seeded = ResultJson::Answers {
        answers: answers_map(serde_json::json!({
            "procedural": {"type": "choice", "choice": "procedural", "confidence": 0.9},
        })),
    };
    checker
        .store
        .append(&NewRow {
            content_sha256: &hash,
            questions_sha256: &question_hash,
            skill: "reviewer",
            path: "/skills/reviewer/SKILL.md",
            source: "typesafe",
            model: "jev-1.0",
            checked_at: "2026-09-25T00:00:00Z",
            result: &seeded,
        })
        .expect("seed the row this run must not duplicate");

    checker
        .run(
            "reviewer".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("SKILL.md"),
        )
        .await;

    let row = checker
        .store
        .latest(&hash, &question_hash)
        .expect("query")
        .expect("row present");
    assert_eq!(row.model, "jev-1.0", "the seeded row must be untouched");
}

#[tokio::test]
async fn display_reports_not_checked_yet_before_any_run() {
    let server = spawn_server(|_head, _body| http_response(200, &clean_answers_body())).await;
    let checker = Checker::open_in_memory(&server.base_url, "key");
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path\noutput: a JSON report\n---\nBody.\n",
    );
    let skill = crate::skill::Skill {
        name: "reviewer".to_string(),
        description: "reviews things".to_string(),
        contract: None,
        directory: dir.path().to_path_buf(),
        path: dir.path().join("SKILL.md"),
    };

    assert_eq!(checker.display(&skill), "not checked yet");
}

#[tokio::test]
async fn display_reports_the_last_failure_reason() {
    let server = spawn_server(|_head, _body| http_response(401, "unauthorized")).await;
    let checker = Checker::open_in_memory(&server.base_url, "key");
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path to review\noutput: a JSON report with pass/fail per file\n---\nRead `input.path` and produce the report.\n",
    );
    let skill = crate::skill::Skill {
        name: "reviewer".to_string(),
        description: "reviews things".to_string(),
        contract: None,
        directory: dir.path().to_path_buf(),
        path: dir.path().join("SKILL.md"),
    };

    checker
        .run(
            "reviewer".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("SKILL.md"),
        )
        .await;

    assert_eq!(
        checker.display(&skill),
        "not checked yet (last attempt: 401)"
    );
}

#[tokio::test]
async fn display_renders_the_stored_rules_result() {
    let server = spawn_server(|_head, _body| panic!("TypeSafe must not be contacted")).await;
    let checker = Checker::open_in_memory(&server.base_url, "key");
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill(
        dir.path(),
        "---\nname: reviewer\ndescription: reviews things\ninput: a path\noutput: the result\n---\nBody.\n",
    );
    let skill = crate::skill::Skill {
        name: "reviewer".to_string(),
        description: "reviews things".to_string(),
        contract: None,
        directory: dir.path().to_path_buf(),
        path: dir.path().join("SKILL.md"),
    };

    checker
        .run(
            "reviewer".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("SKILL.md"),
        )
        .await;

    let text = checker.display(&skill);
    assert!(text.starts_with("rules"), "{text}");
    assert!(text.contains("output_usable=flag"), "{text}");
}
