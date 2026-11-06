use super::*;

#[test]
fn confidence_is_finite_bounded_and_optional() {
    for probability in [None, Some(0.0), Some(1.0), Some(0.5)] {
        assert!(
            Answer {
                choice: "defer".into(),
                probability
            }
            .valid_probability()
        );
    }
    for probability in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
        assert!(
            !Answer {
                choice: "defer".into(),
                probability: Some(probability)
            }
            .valid_probability()
        );
    }
}

fn text_question(max_bytes: usize) -> Question {
    Question {
        id: "explanation",
        instructions: "Explain.".into(),
        kind: Kind::Text { max_bytes },
    }
}

fn choice_question() -> Question {
    Question {
        id: "action",
        instructions: "Pick.".into(),
        kind: Kind::Choice(vec![("retract".into(), "withdraw".into())]),
    }
}

#[test]
fn session_prompt_states_the_text_bound() {
    let text = session::user_text("state", &[text_question(512)]);
    assert!(
        text.contains("\"explanation\": Explain.\nA JSON string of at most 512 bytes.\n"),
        "{text}"
    );
}

#[test]
fn session_parse_accepts_a_string_within_the_bound() {
    let questions = [text_question(5)];
    let answers = session::parse(r#"{"explanation":"abcde"}"#, &questions).expect("within bound");
    assert_eq!(answers["explanation"].choice, "abcde");
    assert_eq!(answers["explanation"].probability, None);
    let empty = session::parse(r#"{"explanation":""}"#, &questions).expect("empty string");
    assert_eq!(empty["explanation"].choice, "");
}

#[test]
fn session_parse_bounds_text_by_bytes_not_characters() {
    let questions = [text_question(5)];
    // Two characters, six bytes.
    let failure = session::parse(r#"{"explanation":"éèé"}"#, &questions).unwrap_err();
    assert!(matches!(failure, Failure::Invalid(_)), "{failure:?}");
}

#[test]
fn session_parse_rejects_a_longer_string_a_non_string_and_a_missing_key() {
    let questions = [text_question(5)];
    for reply in [
        r#"{"explanation":"abcdef"}"#,
        r#"{"explanation":12}"#,
        r#"{"explanation":true}"#,
        r#"{"explanation":null}"#,
        r#"{"explanation":["abc"]}"#,
        r#"{}"#,
        r#"{"other":"abc"}"#,
        r#"{"explanation":"abc","extra":"abc"}"#,
    ] {
        let failure = session::parse(reply, &questions).expect_err(reply);
        assert!(
            matches!(failure, Failure::Invalid(_)),
            "{reply}: {failure:?}"
        );
    }
}

#[test]
fn session_parse_reads_text_beside_choice_and_yes_no_questions() {
    let questions = [
        choice_question(),
        Question {
            id: "evidence_1",
            instructions: "Supports?".into(),
            kind: Kind::YesNo {
                yes: "yes".into(),
                no: "no".into(),
            },
        },
        text_question(64),
    ];
    let answers = session::parse(
        r#"{"action":"retract","evidence_1":true,"explanation":"see evidence_1"}"#,
        &questions,
    )
    .expect("mixed reply");
    assert_eq!(answers["action"].choice, "retract");
    assert_eq!(answers["evidence_1"].choice, YES);
    assert_eq!(answers["explanation"].choice, "see evidence_1");
}

#[tokio::test]
async fn auxiliary_ask_returns_the_text_failure_without_sending() {
    // Port 1 accepts no connection; a request would not return Failure::Error with an empty
    // request field, because a sent request records its payload.
    let backend = jev::Jev::new("http://127.0.0.1:1", "test-key");
    let exchange = backend
        .ask(
            "state",
            &[choice_question(), text_question(16)],
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(exchange.request, "");
    assert_eq!(exchange.response, None);
    match exchange.result {
        Err(Failure::Error(reason)) => assert!(reason.contains("choice and yes/no"), "{reason}"),
        other => panic!("expected Failure::Error, got {other:?}"),
    }
}
