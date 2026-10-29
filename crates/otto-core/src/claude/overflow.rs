//! Narrow Claude context overflow detection; returned metadata never contains
//! arbitrary response text. Output-budget and generic 413 errors are not overflow.
use super::json;
use crate::provider::ContextOverflowError;
pub const MAX_ERROR_BODY: usize = 32 << 10;

pub fn classify_overflow(status: u16, body: &[u8]) -> Option<ContextOverflowError> {
    if status != 400 || body.len() > MAX_ERROR_BODY {
        return None;
    }
    let root = json::parse(body).ok()?;
    let error = root.get("error")?;
    if error.get("type")?.as_str()? != "invalid_request_error" {
        return None;
    }
    let message = error.get("message")?.as_str()?.to_ascii_lowercase();
    let counts = message
        .strip_prefix("prompt is too long:")?
        .trim()
        .split_once(" tokens > ")?;
    let current = counts.0.trim().parse::<i64>().ok()?;
    let maximum = counts
        .1
        .trim()
        .strip_suffix(" maximum")?
        .trim()
        .parse::<i64>()
        .ok()?;
    if current <= maximum || maximum <= 0 {
        return None;
    }
    Some(ContextOverflowError {
        status,
        code: "invalid_request_error".into(),
        current_tokens: current,
        maximum_tokens: maximum,
    })
}
