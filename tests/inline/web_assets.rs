//! Static asset routes serve the right body + `Content-Type`, pure — no
//! server spin-up needed, `serve()` takes a path and returns a response.

use super::*;

fn content_type(response: &tiny_http::Response<std::io::Cursor<Vec<u8>>>) -> String {
    response
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_string())
        .expect("content-type header present")
}

#[test]
fn index_is_served_as_html() {
    let response = serve("/").expect("index route");
    assert_eq!(content_type(&response), "text/html; charset=utf-8");
}

#[test]
fn app_js_is_served_as_javascript() {
    let response = serve("/app.js").expect("app.js route");
    assert_eq!(content_type(&response), "application/javascript");
}

#[test]
fn app_css_is_served_as_css() {
    let response = serve("/app.css").expect("app.css route");
    assert_eq!(content_type(&response), "text/css");
}

#[test]
fn vendored_alpine_is_served_as_javascript() {
    let response = serve("/vendor/alpine.min.js").expect("alpine route");
    assert_eq!(content_type(&response), "application/javascript");
}

#[test]
fn vendored_pico_is_served_as_css() {
    let response = serve("/vendor/pico.min.css").expect("pico route");
    assert_eq!(content_type(&response), "text/css");
}

#[test]
fn unknown_path_is_not_a_static_asset() {
    assert!(serve("/api/status").is_none());
    assert!(serve("/nope").is_none());
}
