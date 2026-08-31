//! Pure state-machine tests for the in-memory job store — no HTTP, no
//! background thread, just `start`/`finish`/`poll` directly.

use super::*;

#[test]
fn a_fresh_job_polls_as_pending() {
    let store = new_store();
    let id = start(&store);
    let (status, body) = poll(&store, &id);
    assert_eq!(status.0, 200);
    assert_eq!(body, r#"{"status":"pending"}"#);
}

#[test]
fn finishing_ok_polls_as_succeeded_with_the_result() {
    let store = new_store();
    let id = start(&store);
    finish(&store, &id, Ok(serde_json::json!({"name": "kitty"})));
    let (status, body) = poll(&store, &id);
    assert_eq!(status.0, 200);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(parsed["status"], "succeeded");
    assert_eq!(parsed["result"]["name"], "kitty");
}

#[test]
fn finishing_err_polls_as_failed_with_the_message() {
    let store = new_store();
    let id = start(&store);
    finish(&store, &id, Err("browser timed out".to_string()));
    let (status, body) = poll(&store, &id);
    assert_eq!(status.0, 200);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(parsed["status"], "failed");
    assert_eq!(parsed["error"], "browser timed out");
}

#[test]
fn polling_an_unknown_id_is_404() {
    let store = new_store();
    let (status, _) = poll(&store, "does-not-exist");
    assert_eq!(status.0, 404);
}

#[test]
fn two_jobs_never_collide() {
    let store = new_store();
    let a = start(&store);
    let b = start(&store);
    assert_ne!(a, b);
}
