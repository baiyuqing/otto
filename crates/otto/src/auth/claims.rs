//! ID token claim extraction. Port of `internal/auth/claims.go`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// The namespaced claim object OpenAI embeds in the ID token. Port of
/// `openaiAuthClaim`.
const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// Extracts `chatgpt_account_id` from an OpenAI ID token.
///
/// The token is a JWT (`header.payload.signature`). Only the payload is
/// decoded; the signature is not verified because the token was just received
/// directly from the OAuth token endpoint over TLS and is used solely to read
/// the account id needed to route requests. The claim is normally nested under
/// the namespaced object; a top-level key is accepted as a fallback.
///
/// The error strings describe the shape of the token, never its contents.
pub fn account_id_from_id_token(id_token: &str) -> Result<String, String> {
    let segments: Vec<&str> = id_token.split('.').collect();
    if segments.len() < 2 {
        return Err(format!(
            "malformed id token: want at least 2 segments, got {}",
            segments.len()
        ));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(segments[1])
        .map_err(|_| "decode id token payload".to_owned())?;
    let claims: serde_json::Map<String, Value> =
        serde_json::from_slice(&payload).map_err(|_| "parse id token claims".to_owned())?;

    if let Some(Value::String(id)) = claims
        .get(OPENAI_AUTH_CLAIM)
        .and_then(|nested| nested.get("chatgpt_account_id"))
        && !id.is_empty()
    {
        return Ok(id.clone());
    }
    if let Some(Value::String(id)) = claims.get("chatgpt_account_id")
        && !id.is_empty()
    {
        return Ok(id.clone());
    }
    Err("id token has no chatgpt_account_id claim".to_owned())
}

#[cfg(test)]
pub(crate) fn fake_id_token(claims: serde_json::Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    format!("{header}.{payload}.sig")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Port of `TestAccountIDFromIDTokenNestedClaim`.
    #[test]
    fn the_namespaced_claim_supplies_the_account_id() {
        let token = fake_id_token(json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-123",
                "chatgpt_plan_type": "pro",
            }
        }));
        assert_eq!(account_id_from_id_token(&token).unwrap(), "acct-123");
    }

    /// Port of `TestAccountIDFromIDTokenTopLevelFallback`.
    #[test]
    fn a_top_level_claim_is_the_fallback() {
        let token = fake_id_token(json!({"chatgpt_account_id": "top-999"}));
        assert_eq!(account_id_from_id_token(&token).unwrap(), "top-999");
    }

    /// Port of `TestAccountIDFromIDTokenMissing` and
    /// `TestAccountIDFromIDTokenMalformed`, plus the empty-value and
    /// wrong-type cases Go's `json.Unmarshal` into a typed struct rejects.
    #[test]
    fn absent_empty_or_malformed_claims_are_errors() {
        for token in [
            fake_id_token(json!({"sub": "user"})),
            fake_id_token(json!({"chatgpt_account_id": ""})),
            fake_id_token(json!({"chatgpt_account_id": 7})),
            fake_id_token(json!({"https://api.openai.com/auth": {"chatgpt_account_id": ""}})),
            fake_id_token(json!(["not an object"])),
            "not-a-jwt".to_owned(),
            "header.!!!not-base64!!!.sig".to_owned(),
        ] {
            assert!(account_id_from_id_token(&token).is_err(), "{token}");
        }
    }

    /// An empty namespaced claim must not shadow a usable top-level one, which
    /// is what Go's "try nested, then top level" order gives.
    #[test]
    fn an_unusable_namespaced_claim_falls_through_to_the_top_level() {
        let token = fake_id_token(json!({
            "https://api.openai.com/auth": {"chatgpt_plan_type": "pro"},
            "chatgpt_account_id": "top-1",
        }));
        assert_eq!(account_id_from_id_token(&token).unwrap(), "top-1");
    }

    /// Error text describes the token's shape and never echoes its bytes.
    #[test]
    fn errors_do_not_echo_the_token() {
        const SECRET: &str = "id-token-secret";
        let token = fake_id_token(json!({"sub": SECRET}));
        let error = account_id_from_id_token(&token).unwrap_err();
        assert!(!error.contains(SECRET), "{error}");
        assert!(!error.contains(&token), "{error}");
    }
}
