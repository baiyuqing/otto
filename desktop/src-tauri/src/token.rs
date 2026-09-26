//! Pulls the `token` query parameter out of the URL `otto serve` announces,
//! for `Authorization: Bearer <token>` on the app's own `POST
//! /v1/workspaces` calls. No `url` crate: the shape is fixed
//! (`http://host:port/?token=...`), so a manual split is enough.

/// `Some(token)` when `url` has a `token` query parameter; `None` otherwise
/// (no query string, or no `token` key in it).
pub fn extract_token(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_token_query_parameter() {
        assert_eq!(
            extract_token("http://127.0.0.1:8787/?token=abc123"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn finds_token_among_other_parameters() {
        assert_eq!(
            extract_token("http://127.0.0.1:8787/?a=1&token=abc123&b=2"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn none_without_a_query_string() {
        assert_eq!(extract_token("http://127.0.0.1:8787/"), None);
    }

    #[test]
    fn none_without_a_token_parameter() {
        assert_eq!(extract_token("http://127.0.0.1:8787/?a=1"), None);
    }
}
