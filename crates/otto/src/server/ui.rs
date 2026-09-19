//! The embedded web UI.
//!
//! The built bundle is read from `ui/dist` (what `make ui` writes) and
//! compiled into the binary at build time. The directory is tracked with
//! only a `.gitkeep`, so a plain checkout still builds and the root then
//! answers with a one-line placeholder.
//!
//! The UI is not part of the API: no token, not in the route table, not in
//! `openapi.yaml`.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use include_dir::{Dir, include_dir};

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist");

/// What `GET /` returns when `make ui` has not run.
pub const PLACEHOLDER: &str = "Web UI not built; run make ui\n";

/// The embedded `index.html`, or `None` on an unbuilt checkout.
pub fn index_page() -> Option<&'static [u8]> {
    DIST.get_file("index.html").map(|file| file.contents())
}

/// One embedded file under `dist`, addressed the way the request path spells
/// it (`assets/app-abc123.js`).
pub fn dist_file(path: &str) -> Option<&'static [u8]> {
    DIST.get_file(path).map(|file| file.contents())
}

/// `GET /`. `Cache-Control: no-cache` is set on both branches, matching Go.
pub fn index_response(page: Option<&[u8]>) -> Response {
    let (content_type, body) = match page {
        Some(page) => ("text/html; charset=utf-8", page.to_vec()),
        None => ("text/plain; charset=utf-8", PLACEHOLDER.as_bytes().to_vec()),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("static header values")
}

/// `GET /assets/...`. A miss is a plain 404, so an unknown path under the UI
/// prefix behaves like any other unknown path.
pub fn asset_response(path: &str, body: Option<&[u8]>) -> Response {
    let Some(body) = body else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("404 page not found\n"))
            .expect("static header values");
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path))
        .body(Body::from(body.to_vec()))
        .expect("static header values")
}

/// The media type Go's `mime.TypeByExtension` returns for the extensions a
/// Vite bundle actually emits. Anything else falls back to the type Go's
/// content sniffer reports for text, which is what the remaining files are.
pub fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map(|(_, tail)| tail).unwrap_or("");
    match extension {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/vnd.microsoft.icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        _ => "text/plain; charset=utf-8",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_type_of(response: &Response) -> &str {
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    #[test]
    fn the_index_serves_the_embedded_page_as_html() {
        let response = index_response(Some(b"<!doctype html><title>otto</title>"));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type_of(&response), "text/html; charset=utf-8");
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-cache")
        );
    }

    #[test]
    fn an_unbuilt_checkout_serves_the_placeholder() {
        let response = index_response(None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type_of(&response), "text/plain; charset=utf-8");
    }

    #[test]
    fn assets_get_their_extension_type_and_a_miss_is_404() {
        let response = asset_response("assets/app-abc123.js", Some(b"console.log(1)"));
        assert_eq!(response.status(), StatusCode::OK);
        assert!(content_type_of(&response).starts_with("text/javascript"));
        assert_eq!(
            asset_response("assets/missing.js", None).status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn the_embed_path_resolves_on_an_unbuilt_checkout() {
        // The checked-in directory holds only .gitkeep, so this asserts the
        // embed path resolves, not that `make ui` has run.
        assert!(DIST.get_file("nothing-here").is_none());
    }

    #[test]
    fn make_build_refreshes_the_embedded_ui_first() {
        let makefile = include_str!("../../../../Makefile");
        let rule = makefile
            .lines()
            .find(|line| line.starts_with("build:"))
            .expect("build rule");
        let dependencies = rule
            .split("##")
            .next()
            .unwrap_or_default()
            .trim_start_matches("build:")
            .split_whitespace();
        assert!(
            dependencies
                .into_iter()
                .any(|dependency| dependency == "ui")
        );
    }
}
