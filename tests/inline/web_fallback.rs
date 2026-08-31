//! End-to-end tests for the Fallback tab write endpoints, driving a real
//! spawned server + real `actions.rs` calls against a sandboxed home.

use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, AppState, ConfigHandle, Profile};
use crate::testutil::HomeSandbox;

fn start_with(config: AppConfig) -> (crate::web::Handle, ConfigHandle) {
    let handle_config: ConfigHandle = Arc::new(RankedMutex::new(config));
    let server =
        crate::web::spawn(Arc::clone(&handle_config), "127.0.0.1:0").expect("server binds");
    (server, handle_config)
}

fn two_member_config() -> AppConfig {
    let a = Profile::new("a".to_string(), None, None);
    let b = Profile::new("b".to_string(), None, None);
    for p in [&a, &b] {
        crate::profile::save_profile(p).expect("save profile");
    }
    let state = AppState {
        profiles: vec!["a".into(), "b".into()],
        fallback_chain: vec!["a".into(), "b".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    AppConfig {
        state,
        profiles: vec![a, b],
    }
}

#[test]
fn set_chain_replaces_membership_and_order() {
    let _home = HomeSandbox::new();
    let (handle, config_handle) = start_with(two_member_config());
    let url = format!("http://{}/api/fallback", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"chain": ["b", "a"]}).to_string())
        .expect("set chain request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    let names: Vec<&str> = cfg
        .state
        .fallback_chain
        .iter()
        .map(|n| n.as_ref())
        .collect();
    assert_eq!(names, vec!["b", "a"]);
    drop(cfg);
    handle.stop();
}

#[test]
fn set_chain_seeds_a_default_threshold_for_a_new_member() {
    let _home = HomeSandbox::new();
    let mut config = two_member_config();
    // Start with an empty chain and no threshold on "a" — adding it back
    // through the endpoint must seed the default, same as the TUI's `+ add`.
    config.state.fallback_chain.clear();
    crate::profile::save_app_state(&config.state).expect("persist state");
    let (handle, config_handle) = start_with(config);

    let url = format!("http://{}/api/fallback", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"chain": ["a"]}).to_string())
        .expect("set chain request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    let a = cfg.find(&"a".into()).expect("still present");
    assert_eq!(
        a.fallback_threshold,
        Some(crate::fallback::DEFAULT_THRESHOLD)
    );
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_member_sets_threshold_and_spend_ceiling() {
    let _home = HomeSandbox::new();
    let (handle, config_handle) = start_with(two_member_config());
    let url = format!("http://{}/api/profiles/a/fallback", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"threshold": 80.0, "max_auto_spend": 12.5}).to_string())
        .expect("patch member request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    let a = cfg.find(&"a".into()).expect("still present");
    assert_eq!(a.fallback_threshold, Some(80.0));
    assert_eq!(a.max_auto_spend, Some(12.5));
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_member_preferred_is_idempotent_and_exclusive() {
    let _home = HomeSandbox::new();
    let (handle, config_handle) = start_with(two_member_config());
    let url_a = format!("http://{}/api/profiles/a/fallback", handle.addr());
    let url_b = format!("http://{}/api/profiles/b/fallback", handle.addr());

    // Turn preferred on for "a".
    let response = ureq::patch(&url_a)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"preferred": true}).to_string())
        .expect("set preferred on a");
    assert_eq!(response.status().as_u16(), 200);

    // Sending the same desired value again must be a no-op (idempotent), not
    // a second flip that would turn it back off.
    let response = ureq::patch(&url_a)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"preferred": true}).to_string())
        .expect("repeat set preferred on a");
    assert_eq!(response.status().as_u16(), 200);
    {
        #[allow(clippy::unwrap_used, reason = "test-only")]
        let cfg = config_handle.lock().unwrap();
        assert!(cfg.find(&"a".into()).expect("present").preferred);
    }

    // Turning it on for "b" must exclusively clear it from "a" (radio).
    let response = ureq::patch(&url_b)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"preferred": true}).to_string())
        .expect("set preferred on b");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    assert!(!cfg.find(&"a".into()).expect("present").preferred);
    assert!(cfg.find(&"b".into()).expect("present").preferred);
    drop(cfg);
    handle.stop();
}

#[test]
fn list_reports_chain_members_and_non_member_candidates() {
    let _home = HomeSandbox::new();
    let mut config = two_member_config();
    let c = Profile::new("c".to_string(), None, None);
    crate::profile::save_profile(&c).expect("save profile");
    config.state.profiles.push("c".into());
    config.profiles.push(c);
    crate::profile::save_app_state(&config.state).expect("persist state");

    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/fallback", handle.addr());
    let mut response = ureq::get(&url).call().expect("list request");
    assert_eq!(response.status().as_u16(), 200);
    let text = response.body_mut().read_to_string().expect("body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("json body");
    let names: Vec<&str> = body["chain"]
        .as_array()
        .expect("chain array")
        .iter()
        .map(|m| m["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["a", "b"]);
    assert_eq!(
        body["chain"][0]["threshold"],
        crate::fallback::DEFAULT_THRESHOLD
    );
    assert_eq!(body["candidates"], serde_json::json!(["c"]));
    handle.stop();
}
