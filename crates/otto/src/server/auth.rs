//! Bearer-token gating for the `/v1/` routes.
//!
//! Any web page a browser has open can send requests to a loopback port, but
//! the browser never attaches our `Authorization` header on behalf of another
//! origin. Only the page that received the token from the `otto serve` startup
//! URL can therefore reach `/v1/`, which covers both CSRF and DNS rebinding
//! without CORS or `Origin` checks. The token is accepted from the header only:
//! a query parameter would end up in proxy access logs and `Referer` headers.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;

use super::error_response;

/// True when `headers` carries exactly `Authorization: Bearer <token>`.
pub fn authorized(token: &str, headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(presented.as_bytes(), token.as_bytes())
}

/// The 401 an unauthenticated request gets, including `WWW-Authenticate:
/// Bearer`.
pub fn unauthorized() -> Response {
    let mut response = error_response(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "missing or invalid token",
    );
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, "Bearer".parse().expect("ascii"));
    response
}

/// Constant-time comparison: unequal lengths are rejected without comparing,
/// and equal lengths are compared with no early exit, so the comparison time
/// does not depend on how many leading bytes matched.
fn constant_time_eq(presented: &[u8], expected: &[u8]) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in presented.iter().zip(expected) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !value.is_empty() {
            headers.insert(header::AUTHORIZATION, value.parse().expect("ascii"));
        }
        headers
    }

    #[test]
    fn only_the_exact_bearer_token_is_accepted() {
        let token = "test-token-0123456789abcdef";
        assert!(authorized(token, &headers(&format!("Bearer {token}"))));
        for bad in [
            "",
            "Bearer wrong",
            "Bearer test-token-0123456789abcdefx",
            "Basic test-token-0123456789abcdef",
            "test-token-0123456789abcdef",
            "Bearer",
        ] {
            assert!(!authorized(token, &headers(bad)), "accepted {bad:?}");
        }
    }

    #[test]
    fn constant_time_eq_matches_byte_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_challenge_names_the_bearer_scheme() {
        let response = unauthorized();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer")
        );
    }
}
