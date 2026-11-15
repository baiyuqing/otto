//! The auxiliary Jev model as an advice [`Backend`].
//!
//! Used when `TYPESAFE_API_KEY` is set. A choice question is a TypeSafe
//! `choice` question whose answer carries `confidence`; a yes/no question is a
//! `noul` question whose number is the probability of yes, so a `noul` below
//! 0.5 answers `no` with probability `1 - noul`.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{Answer, Backend, Exchange, Failure, Kind, NO, Question, YES};
use crate::typesafe::{self, Client, Error, WireQuestion};

pub(crate) struct Jev {
    client: Client,
}

impl Jev {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            client: Client::new(base_url, api_key),
        }
    }
}

/// The auxiliary model answers choice and yes/no questions only.
const TEXT_UNSUPPORTED: &str = "the auxiliary model answers choice and yes/no questions only";

fn wire(questions: &[Question]) -> Result<Map<String, Value>, Failure> {
    let mut wired = Map::new();
    for question in questions {
        let (kind, criteria) = match &question.kind {
            Kind::Choice(options) => (
                "choice",
                options
                    .iter()
                    .map(|(id, meaning)| (id.clone(), Value::from(meaning.as_str())))
                    .collect(),
            ),
            Kind::YesNo { yes, no } => ("noul", json!({"true": yes, "false": no})),
            Kind::Text { .. } => return Err(Failure::Error(TEXT_UNSUPPORTED.to_owned())),
        };
        let wire = WireQuestion {
            r#type: kind,
            instructions: &question.instructions,
            criteria,
        };
        wired.insert(
            question.id.to_owned(),
            serde_json::to_value(wire).expect("WireQuestion serializes"),
        );
    }
    Ok(wired)
}

fn probability(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|probability| (0.0..=1.0).contains(probability))
}

fn decode(
    answers: &typesafe::AnswerMap,
    questions: &[Question],
) -> Result<HashMap<String, Answer>, Failure> {
    let invalid = |reason: &str| Failure::Invalid(reason.to_owned());
    if answers.len() != questions.len() {
        return Err(invalid("the reply keys differ from the questions"));
    }
    let mut decoded = HashMap::new();
    for question in questions {
        let answer = answers
            .get(question.id)
            .ok_or_else(|| invalid("an answer is missing"))?;
        let kind = answer.get("type").and_then(Value::as_str);
        let decoded_answer = match (&question.kind, kind) {
            (Kind::Choice(options), Some("choice")) => {
                let choice = answer
                    .get("choice")
                    .and_then(Value::as_str)
                    .filter(|choice| options.iter().any(|(id, _)| id == choice))
                    .ok_or_else(|| invalid("a choice is not one of the options"))?;
                Answer {
                    choice: choice.to_owned(),
                    probability: match answer.get("confidence") {
                        None => None,
                        Some(value) => Some(
                            probability(value)
                                .ok_or_else(|| invalid("confidence is not a probability"))?,
                        ),
                    },
                }
            }
            (Kind::YesNo { .. }, Some("noul")) => {
                let yes = answer
                    .get("noul")
                    .and_then(probability)
                    .ok_or_else(|| invalid("a noul answer is not a probability"))?;
                if yes >= 0.5 {
                    Answer {
                        choice: YES.to_owned(),
                        probability: Some(yes),
                    }
                } else {
                    Answer {
                        choice: NO.to_owned(),
                        probability: Some(1.0 - yes),
                    }
                }
            }
            _ => return Err(invalid("an answer has the wrong type")),
        };
        decoded.insert(question.id.to_owned(), decoded_answer);
    }
    Ok(decoded)
}

#[async_trait::async_trait]
impl Backend for Jev {
    fn model(&self) -> String {
        typesafe::MODEL.to_owned()
    }

    fn auxiliary(&self) -> bool {
        true
    }

    async fn ask(&self, state: &str, questions: &[Question], limit: Duration) -> Exchange {
        let wired = match wire(questions) {
            Ok(wired) => wired,
            Err(failure) => {
                return Exchange {
                    request: String::new(),
                    response: None,
                    result: Err(failure),
                };
            }
        };
        let payload = typesafe::request_body(state, &wired);
        let sent = tokio::time::timeout(limit, self.client.post(&payload)).await;
        let (response, result) = match sent {
            Err(_) => (None, Err(Failure::Timeout)),
            Ok(Err(Error::Timeout)) => (None, Err(Failure::Timeout)),
            Ok(Err(error)) => (None, Err(Failure::Error(error.reason()))),
            Ok(Ok(reply)) => {
                let result = decode(&reply.answers, questions);
                (Some(reply.raw), result)
            }
        };
        Exchange {
            request: payload,
            response,
            result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_choice_answers_and_confidence() {
        let questions = [Question {
            id: "timing",
            instructions: "schedule only".into(),
            kind: Kind::Choice(vec![("defer".into(), "retain evidence".into())]),
        }];
        for confidence in [json!(0), json!(1), json!(0.5)] {
            let answers = serde_json::from_value(json!({
                "timing": {"type":"choice", "choice":"defer", "confidence":confidence}
            }))
            .unwrap();
            assert!(decode(&answers, &questions).is_ok());
        }
        for confidence in [json!(-0.1), json!(1.1), json!("0.5"), Value::Null] {
            let answers = serde_json::from_value(json!({
                "timing": {"type":"choice", "choice":"defer", "confidence":confidence}
            }))
            .unwrap();
            assert!(matches!(
                decode(&answers, &questions),
                Err(Failure::Invalid(_))
            ));
        }
        for invalid in [
            json!({}),
            json!({"timing":{"type":"choice", "choice":"grant"}}),
            json!({"timing":{"type":"choice", "choice":"defer"}, "extra":{}}),
            json!({"timing":{"type":"noul", "noul":0.8}}),
        ] {
            let answers = serde_json::from_value(invalid).unwrap();
            assert!(matches!(
                decode(&answers, &questions),
                Err(Failure::Invalid(_))
            ));
        }
        let no_confidence = serde_json::from_value(json!({
            "timing":{"type":"choice", "choice":"defer"}
        }))
        .unwrap();
        assert_eq!(
            decode(&no_confidence, &questions).unwrap()["timing"].probability,
            None
        );
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
            kind: Kind::Choice(vec![("defer".into(), "retain".into())]),
        }
    }

    #[test]
    fn wire_refuses_a_question_set_with_a_text_question() {
        for questions in [
            vec![text_question(16)],
            vec![choice_question(), text_question(16)],
            vec![text_question(16), choice_question()],
        ] {
            let failure = wire(&questions).expect_err("text is unsupported");
            match failure {
                Failure::Error(reason) => assert!(reason.contains("choice and yes/no"), "{reason}"),
                other => panic!("expected Failure::Error, got {other:?}"),
            }
        }
        assert!(wire(&[choice_question()]).is_ok());
    }
}
