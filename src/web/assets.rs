//! Static dashboard assets, embedded into the binary at compile time —
//! `assets/web/` ships alongside the source, never read from disk at
//! runtime. Alpine.js and Pico.css are vendored files under `vendor/`
//! (downloaded once, committed to the repo), not fetched from a CDN.
//!
//! Served outside the JSON API's `route()`/`resolve()` pipeline (see
//! [`serve`]): every `/api/*` route answers `application/json`, but these
//! five paths each need their own `Content-Type`, so they get a dedicated
//! lookup in `handle_request` instead of forcing [`super::RouteResult`] to
//! carry a content type only these five callers would ever set.

use std::io::Cursor;

use tiny_http::{Header, Response};

const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
const APP_JS: &str = include_str!("../../assets/web/app.js");
const APP_CSS: &str = include_str!("../../assets/web/app.css");
const ALPINE_JS: &str = include_str!("../../assets/web/vendor/alpine.min.js");
const PICO_CSS: &str = include_str!("../../assets/web/vendor/pico.min.css");

/// `path` is the request URL with any query string already stripped. `None`
/// for anything that isn't one of the five static assets, so the caller
/// falls through to the `/api/*` route table.
pub(super) fn serve(path: &str) -> Option<Response<Cursor<Vec<u8>>>> {
    let (body, content_type) = match path {
        "/" => (INDEX_HTML, "text/html; charset=utf-8"),
        "/app.js" => (APP_JS, "application/javascript"),
        "/app.css" => (APP_CSS, "text/css"),
        "/vendor/alpine.min.js" => (ALPINE_JS, "application/javascript"),
        "/vendor/pico.min.css" => (PICO_CSS, "text/css"),
        _ => return None,
    };
    Some(with_content_type(body, content_type))
}

#[allow(
    clippy::expect_used,
    reason = "the header name/value are both compile-time-constant ASCII literals"
)]
fn with_content_type(body: &str, content_type: &str) -> Response<Cursor<Vec<u8>>> {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("static header name/value are always valid");
    Response::from_string(body.to_string()).with_header(header)
}

#[cfg(test)]
#[path = "../../tests/inline/web_assets.rs"]
mod tests;
