//! End-to-end tests for the login endpoints' PRE-FLIGHT validation only.
//!
//! The success path of both `start_oauth` and `start_alibaba` opens a real
//! system browser and blocks a background thread on a real network round
//! trip (to `platform.claude.com` / an Alibaba console) — never something an
//! automated test should trigger. What's safe and worth covering here is
//! everything that must reject BEFORE any of that starts: duplicate/invalid
//! names, an unknown profile, an unrecognized site — each of these returns
//! synchronously with no thread spawned and no job created.

use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, AppState, ConfigHandle, Profile};
use crate::testutil::HomeSandbox;

const TEST_TOKEN: &str = "test-token-0123456789";

fn start_with(config: AppConfig) -> (crate::web::Handle, ConfigHandle) {
    let handle_config: ConfigHandle = Arc::new(RankedMutex::new(config));
    let server = crate::web::spawn(
        Arc::clone(&handle_config),
        TEST_TOKEN.to_string(),
        "127.0.0.1:0",
    )
    .expect("server binds");
    (server, handle_config)
}

#[test]
fn oauth_login_rejects_a_duplicate_name_before_opening_a_browser() {
    let _home = HomeSandbox::new();
    let existing = Profile::new("taken".to_string(), None, None);
    crate::profile::save_profile(&existing).expect("save profile");
    let state = AppState {
        profiles: vec!["taken".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![existing],
    };

    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/login/oauth", handle.addr());
    let err = ureq::post(&url)
        .header("Authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"name": "taken"}).to_string())
        .expect_err("duplicate name");
    let ureq::Error::StatusCode(422) = err else {
        panic!("expected 422, got {err:?}");
    };
    handle.stop();
}

#[test]
fn oauth_login_requires_a_token() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/login/oauth", handle.addr());
    let err = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"name": "new-acct"}).to_string())
        .expect_err("no token");
    let ureq::Error::StatusCode(401) = err else {
        panic!("expected 401, got {err:?}");
    };
    handle.stop();
}

#[test]
fn alibaba_login_rejects_an_unknown_profile_before_opening_a_browser() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/ghost/login/alibaba", handle.addr());
    let err = ureq::post(&url)
        .header("Authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"site": "domestic", "region": "cn-beijing"}).to_string())
        .expect_err("unknown profile");
    let ureq::Error::StatusCode(404) = err else {
        panic!("expected 404, got {err:?}");
    };
    handle.stop();
}

#[test]
fn alibaba_login_rejects_an_unrecognized_site() {
    let _home = HomeSandbox::new();
    let target = Profile::new("acct".to_string(), None, None);
    crate::profile::save_profile(&target).expect("save profile");
    let state = AppState {
        profiles: vec!["acct".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![target],
    };

    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/acct/login/alibaba", handle.addr());
    let err = ureq::post(&url)
        .header("Authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"site": "mars", "region": "cn-beijing"}).to_string())
        .expect_err("unrecognized site");
    let ureq::Error::StatusCode(400) = err else {
        panic!("expected 400, got {err:?}");
    };
    handle.stop();
}

#[test]
fn unknown_job_id_is_404() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/jobs/does-not-exist", handle.addr());
    let err = ureq::get(&url).call().expect_err("unknown job");
    let ureq::Error::StatusCode(404) = err else {
        panic!("expected 404, got {err:?}");
    };
    handle.stop();
}
