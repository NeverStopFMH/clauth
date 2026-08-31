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

fn start() -> Handle {
    spawn(empty_config(), "127.0.0.1:0").expect("server binds")
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
fn write_method_with_no_header_at_all_reaches_the_route_layer() {
    let handle = start();
    let url = format!("http://{}/api/anything", handle.addr());
    let err = ureq::post(&url)
        .send_empty()
        .expect_err("route doesn't exist, but no auth gate should intercept it first");
    // 404, not 401: there is no auth gate anymore, just no handler for this path.
    let ureq::Error::StatusCode(404) = err else {
        panic!("expected 404 (no auth gate, no route), got {err:?}");
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

/// Throwaway manual harness for eyeballing the dashboard in a real browser —
/// not part of the regression suite. Run with:
/// `cargo test --bin clauth web::tests::manual_browser_check -- --ignored --nocapture`
/// then open http://127.0.0.1:47893/.
#[test]
#[ignore = "manual-only: seeds fake data and blocks so a browser can hit it"]
fn manual_browser_check() {
    use crate::profile::{ClaudeCredentials, OAuthToken, Profile};
    use crate::usage::{UsageInfo, UsageWindow};

    let _home = crate::testutil::HomeSandbox::new();

    let mut alpha = Profile::new("alpha".to_string(), None, None);
    alpha.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "alpha-access".into(),
            refresh_token: Some("alpha-refresh".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    alpha.usage = Some(UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 42.0,
            resets_at: Some("2099-01-01T00:00:00Z".into()),
        }),
        seven_day: Some(UsageWindow {
            utilization: 18.0,
            resets_at: Some("2099-01-05T00:00:00Z".into()),
        }),
        ..Default::default()
    });

    let mut beta = Profile::new("beta".to_string(), None, None);
    beta.usage = Some(UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 88.0,
            resets_at: Some("2099-01-01T01:00:00Z".into()),
        }),
        ..Default::default()
    });
    beta.fallback_threshold = Some(95.0);

    let gamma = Profile::new("gamma".to_string(), None, None);

    for p in [&alpha, &beta, &gamma] {
        crate::profile::save_profile(p).expect("save profile");
    }
    let state = AppState {
        profiles: vec!["alpha".into(), "beta".into(), "gamma".into()],
        active_profile: Some("alpha".into()),
        fallback_chain: vec!["alpha".into(), "beta".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![alpha, beta, gamma],
    };

    // GET /api/status is served verbatim from status.json, so seed it by hand
    // rather than driving the full scheduler/cache pipeline this test doesn't
    // need.
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("create clauth dir");
    let status_body = serde_json::json!({
        "schema": 1,
        "generated_at": "2099-01-01T00:00:00Z",
        "active_profile": "alpha",
        "pending_switch": null,
        "wrap_off": false,
        "refresh_interval_ms": 60000,
        "profiles": [
            {
                "name": "alpha", "active": true, "rolling_token": false,
                "provider": "anthropic", "base_url": null, "tier": "Max 5x",
                "has_live_session": true, "auth_status": "ok",
                "fetch_status": "Fresh", "stale": false,
                "fetched_at": "2099-01-01T00:00:00Z", "next_refresh_at": null,
                "auto_start": false, "bell_threshold": null,
                "fallback": {"position": 1, "threshold": 90.0, "armed": true},
                "windows": [
                    {"label": "5h", "utilization_pct": 42.0, "resets_at": "2099-01-01T00:00:00Z"},
                    {"label": "7d", "utilization_pct": 18.0, "resets_at": "2099-01-05T00:00:00Z"}
                ],
                "third_party": null
            },
            {
                "name": "beta", "active": false, "rolling_token": false,
                "provider": "anthropic", "base_url": null, "tier": "Pro",
                "has_live_session": false, "auth_status": "ok",
                "fetch_status": "Cached", "stale": false,
                "fetched_at": "2099-01-01T00:00:00Z", "next_refresh_at": null,
                "auto_start": false, "bell_threshold": null,
                "fallback": {"position": 2, "threshold": 95.0, "armed": false},
                "windows": [
                    {"label": "5h", "utilization_pct": 88.0, "resets_at": "2099-01-01T01:00:00Z"}
                ],
                "third_party": null
            },
            {
                "name": "gamma", "active": false, "rolling_token": false,
                "provider": "anthropic", "base_url": null, "tier": null,
                "has_live_session": false, "auth_status": "broken",
                "fetch_status": null, "stale": false,
                "fetched_at": null, "next_refresh_at": null,
                "auto_start": false, "bell_threshold": null,
                "fallback": null,
                "windows": [],
                "third_party": null
            }
        ]
    });
    crate::profile::atomic_write_600(
        &dir.join(crate::daemon::STATUS_FILE),
        status_body.to_string(),
    )
    .expect("seed status.json");

    let handle_config: crate::profile::ConfigHandle =
        std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
    let handle = spawn(handle_config, "127.0.0.1:47893").expect("server binds");
    println!("dashboard listening on http://{}/", handle.addr());
    std::thread::sleep(std::time::Duration::from_secs(600));
    handle.stop();
}
