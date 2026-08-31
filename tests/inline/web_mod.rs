//! End-to-end HTTP tests against a real `spawn`ed server on an OS-assigned
//! port (`127.0.0.1:0`), so parallel test runs never collide on a fixed one.

use std::sync::Arc;

use super::*;
use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, AppState};

fn empty_config() -> crate::profile::ConfigHandle {
    Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    }))
}

const TEST_TOKEN: &str = "test-token-0123456789";

fn start() -> Handle {
    spawn(empty_config(), TEST_TOKEN.to_string(), "127.0.0.1:0").expect("server binds")
}

#[test]
fn health_check_is_open_and_returns_ok() {
    let handle = start();
    let url = format!("http://{}/api/health", handle.addr());
    let mut response = ureq::get(&url).call().expect("health request");
    assert_eq!(response.status().as_u16(), 200);
    let body = response.body_mut().read_to_string().expect("body");
    assert_eq!(body, r#"{"ok":true}"#);
    handle.stop();
}

#[test]
fn write_method_without_token_is_rejected() {
    let handle = start();
    let url = format!("http://{}/api/anything", handle.addr());
    let err = ureq::post(&url)
        .send_empty()
        .expect_err("no token, must 401");
    let ureq::Error::StatusCode(401) = err else {
        panic!("expected 401, got {err:?}");
    };
    handle.stop();
}

#[test]
fn write_method_with_correct_token_passes_auth() {
    let handle = start();
    let url = format!("http://{}/api/anything", handle.addr());
    let err = ureq::post(&url)
        .header("Authorization", &format!("Bearer {TEST_TOKEN}"))
        .send_empty()
        .expect_err("route doesn't exist yet, but auth must pass first");
    // 404, not 401: the token was accepted, there is just no handler for this
    // path yet (this route lands in a follow-up slice).
    let ureq::Error::StatusCode(404) = err else {
        panic!("expected 404 (auth passed, no route), got {err:?}");
    };
    handle.stop();
}

#[test]
fn status_endpoint_is_503_before_the_first_daemon_tick_writes_the_file() {
    let _home = crate::testutil::HomeSandbox::new();
    let handle = start();
    let url = format!("http://{}/api/status", handle.addr());
    let err = ureq::get(&url).call().expect_err("no status.json yet");
    let ureq::Error::StatusCode(503) = err else {
        panic!("expected 503, got {err:?}");
    };
    handle.stop();
}

#[test]
fn status_endpoint_serves_the_daemons_status_json_verbatim() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("create clauth dir");
    let body = r#"{"schema":1,"active_profile":"kitty"}"#;
    crate::profile::atomic_write_600(&dir.join(crate::daemon::STATUS_FILE), body)
        .expect("seed status.json");

    let handle = start();
    let url = format!("http://{}/api/status", handle.addr());
    let mut response = ureq::get(&url).call().expect("status request");
    assert_eq!(response.status().as_u16(), 200);
    let received = response.body_mut().read_to_string().expect("body");
    assert_eq!(received, body, "served verbatim, not rebuilt");
    handle.stop();
}

#[test]
fn incidents_endpoint_is_503_before_anything_is_cached() {
    let _home = crate::testutil::HomeSandbox::new();
    let handle = start();
    let url = format!("http://{}/api/status/incidents", handle.addr());
    let err = ureq::get(&url).call().expect_err("no cache yet");
    let ureq::Error::StatusCode(503) = err else {
        panic!("expected 503, got {err:?}");
    };
    handle.stop();
}

#[test]
fn incidents_endpoint_serves_the_cache_verbatim() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("create clauth dir");
    let body = r#"{"fetched_at_ms":0,"incidents":[]}"#;
    let path = crate::status::cache_path().expect("cache path");
    crate::profile::atomic_write_600(&path, body).expect("seed cache");

    let handle = start();
    let url = format!("http://{}/api/status/incidents", handle.addr());
    let mut response = ureq::get(&url).call().expect("incidents request");
    assert_eq!(response.status().as_u16(), 200);
    let received = response.body_mut().read_to_string().expect("body");
    assert_eq!(received, body);
    handle.stop();
}

#[test]
fn unknown_get_route_is_a_plain_404_no_auth_involved() {
    let handle = start();
    let url = format!("http://{}/api/does-not-exist", handle.addr());
    let err = ureq::get(&url).call().expect_err("unknown route");
    let ureq::Error::StatusCode(404) = err else {
        panic!("expected 404, got {err:?}");
    };
    handle.stop();
}
