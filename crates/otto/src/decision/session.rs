//! The session-model side of advice: the prompt it is given and the strict
//! parser of its answer. The backend that sends the prompt is built at the
//! composition root, where the provider client lives.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::{Answer, Failure, Kind, NO, Question, YES};

/// The system prompt of an advice request. Fixed text.
pub const SYSTEM_PROMPT: &str = "You give advisory hints on a memory candidate that a person will \
    review. You never decide anything. Reply with one JSON object and no \
    other text: no code fence, no explanation. Use exactly the keys named \
    in the questions.";

/// The user text: the candidate state, then each question with its allowed
/// values.
pub fn user_text(state: &str, questions: &[Question]) -> String {
    let mut text =
        format!("{state}\n\nQuestions. The reply is one JSON object with one key per question.\n");
    for question in questions {
        let _ = write!(text, "\n\"{}\": {}\n", question.id, question.instructions);
        match &question.kind {
            Kind::Choice(options) => {
                text.push_str("A JSON string, exactly one of:\n");
                for (id, meaning) in options {
                    let _ = writeln!(text, "- \"{id}\": {meaning}");
                }
            }
            Kind::YesNo { yes, no } => {
                let _ = writeln!(text, "A JSON boolean:\n- true: {yes}\n- false: {no}");
            }
        }
    }
    text
}

/// Parses `text` as the reply to `questions`. The reply must be one JSON
/// object with exactly the question ids as keys, a string from the options for
/// a choice, and a boolean for a yes/no question; anything else is invalid.
/// A session model reports no probability.
pub fn parse(text: &str, questions: &[Question]) -> Result<HashMap<String, Answer>, Failure> {
    let invalid = |reason: &str| Failure::Invalid(reason.to_owned());
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|_| invalid("the reply is not JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("the reply is not a JSON object"))?;
    if object.len() != questions.len() {
        return Err(invalid("the reply keys differ from the questions"));
    }
    let mut answers = HashMap::new();
    for question in questions {
        let value = object
            .get(question.id)
            .ok_or_else(|| invalid("the reply lacks a question key"))?;
        let choice = match (&question.kind, value) {
            (Kind::Choice(options), serde_json::Value::String(choice))
                if options.iter().any(|(id, _)| id == choice) =>
            {
                choice.clone()
            }
            (Kind::YesNo { .. }, serde_json::Value::Bool(answer)) => {
                (if *answer { YES } else { NO }).to_owned()
            }
            _ => return Err(invalid("an answer is not one of the allowed values")),
        };
        answers.insert(
            question.id.to_owned(),
            Answer {
                choice,
                probability: None,
            },
        );
    }
    Ok(answers)
}
