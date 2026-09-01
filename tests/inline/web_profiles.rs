//! End-to-end tests for the profile endpoints: each drives a real spawned
//! server, hitting real `actions.rs` calls against a sandboxed home, not
//! just the route-dispatch logic in isolation.

use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, AppState, ConfigHandle, Profile};
use crate::testutil::HomeSandbox;

/// Like `web::tests::start`, but keeps an `Arc` clone of the config so a test
/// can inspect the mutated state after the HTTP round trip.
fn start_with(config: AppConfig) -> (crate::web::Handle, ConfigHandle) {
    let handle_config: ConfigHandle = Arc::new(RankedMutex::new(config));
    let server =
        crate::web::spawn(Arc::clone(&handle_config), "127.0.0.1:0").expect("server binds");
    (server, handle_config)
}

fn profile_with_credentials(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save profile");
    p
}

#[test]
fn switch_moves_active_profile_and_persists() {
    let _home = HomeSandbox::new();
    let alpha = profile_with_credentials("alpha");
    let beta = profile_with_credentials("beta");
    let state = AppState {
        profiles: vec!["alpha".into(), "beta".into()],
        active_profile: Some("alpha".into()),
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![alpha, beta],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/switch", handle.addr());
    let response = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"name": "beta"}).to_string())
        .expect("switch request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    assert_eq!(cfg.state.active_profile.as_deref(), Some("beta"));
    drop(cfg);
    handle.stop();
}

#[test]
fn switch_to_an_unknown_profile_is_a_422_and_leaves_state_untouched() {
    let _home = HomeSandbox::new();
    let alpha = profile_with_credentials("alpha");
    let state = AppState {
        profiles: vec!["alpha".into()],
        active_profile: Some("alpha".into()),
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![alpha],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/switch", handle.addr());
    let err = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"name": "ghost"}).to_string())
        .expect_err("no such profile");
    let ureq::Error::StatusCode(422) = err else {
        panic!("expected 422, got {err:?}");
    };

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    assert_eq!(cfg.state.active_profile.as_deref(), Some("alpha"));
    drop(cfg);
    handle.stop();
}

#[test]
fn reorder_moves_a_profile_and_persists() {
    let _home = HomeSandbox::new();
    let a = Profile::new("a".to_string(), None, None);
    let b = Profile::new("b".to_string(), None, None);
    let c = Profile::new("c".to_string(), None, None);
    for p in [&a, &b, &c] {
        crate::profile::save_profile(p).expect("save profile");
    }
    let state = AppState {
        profiles: vec!["a".into(), "b".into(), "c".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![a, b, c],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/reorder", handle.addr());
    let response = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"from": 0, "to": 2}).to_string())
        .expect("reorder request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    let names: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["b", "c", "a"]);
    drop(cfg);
    handle.stop();
}

#[test]
fn create_adds_an_api_key_profile_and_persists() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles", handle.addr());
    let response = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(
            serde_json::json!({
                "name": "sandbox-test",
                "base_url": "https://api.example.com",
                "api_key": "sk-test-0000",
            })
            .to_string(),
        )
        .expect("create request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    let created = cfg
        .profiles
        .iter()
        .find(|p| p.name == "sandbox-test")
        .expect("created profile present");
    assert_eq!(created.base_url.as_deref(), Some("https://api.example.com"));
    assert_eq!(created.api_key.as_deref(), Some("sk-test-0000"));
    drop(cfg);
    handle.stop();
}

#[test]
fn create_with_a_duplicate_name_is_rejected() {
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
    let url = format!("http://{}/api/profiles", handle.addr());
    let err = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"name": "taken"}).to_string())
        .expect_err("duplicate name");
    let ureq::Error::StatusCode(422) = err else {
        panic!("expected 422, got {err:?}");
    };
    handle.stop();
}

#[test]
fn delete_removes_an_inactive_profile() {
    let _home = HomeSandbox::new();
    let doomed = Profile::new("doomed".to_string(), None, None);
    crate::profile::save_profile(&doomed).expect("save profile");
    let state = AppState {
        profiles: vec!["doomed".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![doomed],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/doomed", handle.addr());
    let response = ureq::delete(&url).call().expect("delete request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    assert!(cfg.profiles.iter().all(|p| p.name != "doomed"));
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_sets_custom_env_vars() {
    let _home = HomeSandbox::new();
    let target = Profile::new("envtest".to_string(), None, None);
    crate::profile::save_profile(&target).expect("save profile");
    let state = AppState {
        profiles: vec!["envtest".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![target],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/envtest", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"env": {"CUSTOM_FLAG": "1"}}).to_string())
        .expect("patch request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    let updated = cfg.find(&"envtest".into()).expect("profile still exists");
    assert_eq!(
        updated.env.get("CUSTOM_FLAG").map(String::as_str),
        Some("1")
    );
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_disables_and_reenables_a_profile() {
    let _home = HomeSandbox::new();
    let target = Profile::new("toggleme".to_string(), None, None);
    crate::profile::save_profile(&target).expect("save profile");
    let state = AppState {
        profiles: vec!["toggleme".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![target],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/toggleme", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"disabled": true}).to_string())
        .expect("disable request");
    assert_eq!(response.status().as_u16(), 200);
    {
        #[allow(clippy::unwrap_used, reason = "test-only")]
        let cfg = config_handle.lock().unwrap();
        assert!(
            cfg.find(&"toggleme".into())
                .expect("still present")
                .disabled
        );
    }

    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"disabled": false}).to_string())
        .expect("enable request");
    assert_eq!(response.status().as_u16(), 200);
    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    assert!(
        !cfg.find(&"toggleme".into())
            .expect("still present")
            .disabled
    );
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_sets_model_routing() {
    let _home = HomeSandbox::new();
    let target = Profile::new("modeltest".to_string(), None, None);
    crate::profile::save_profile(&target).expect("save profile");
    let state = AppState {
        profiles: vec!["modeltest".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![target],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/modeltest", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"model": {"default": "claude-opus-4-6", "haiku": "claude-haiku-4-5"}}).to_string())
        .expect("model patch request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    let updated = cfg.find(&"modeltest".into()).expect("still present");
    assert_eq!(updated.models.default.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(updated.models.haiku.as_deref(), Some("claude-haiku-4-5"));
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_renames_a_profile() {
    let _home = HomeSandbox::new();
    let target = Profile::new("oldname".to_string(), None, None);
    crate::profile::save_profile(&target).expect("save profile");
    let state = AppState {
        profiles: vec!["oldname".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![target],
    };

    let (handle, config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/oldname", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"rename": "newname"}).to_string())
        .expect("rename request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    assert!(cfg.find(&"oldname".into()).is_none());
    assert!(cfg.find(&"newname".into()).is_some());
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_rejects_a_rename_onto_an_existing_name() {
    let _home = HomeSandbox::new();
    let a = Profile::new("a".to_string(), None, None);
    let b = Profile::new("b".to_string(), None, None);
    for p in [&a, &b] {
        crate::profile::save_profile(p).expect("save profile");
    }
    let state = AppState {
        profiles: vec!["a".into(), "b".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![a, b],
    };

    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles/a", handle.addr());
    let err = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"rename": "b"}).to_string())
        .expect_err("duplicate rename target");
    let ureq::Error::StatusCode(422) = err else {
        panic!("expected 422, got {err:?}");
    };
    handle.stop();
}

#[test]
fn list_includes_disabled_profiles_with_setup_only_fields() {
    let _home = HomeSandbox::new();
    let mut visible = Profile::new("visible".to_string(), None, None);
    visible.base_url = Some("https://api.example.com".into());
    visible.api_key = Some("sk-secret".into());
    let mut hidden = Profile::new("hidden".to_string(), None, None);
    hidden.disabled = true;
    for p in [&visible, &hidden] {
        crate::profile::save_profile(p).expect("save profile");
    }
    let state = AppState {
        profiles: vec!["visible".into(), "hidden".into()],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let config = AppConfig {
        state,
        profiles: vec![visible, hidden],
    };

    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles", handle.addr());
    let mut response = ureq::get(&url).call().expect("list request");
    assert_eq!(response.status().as_u16(), 200);
    let text = response.body_mut().read_to_string().expect("body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("json body");
    let names: Vec<&str> = body["profiles"]
        .as_array()
        .expect("profiles array")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["visible", "hidden"]);
    assert_eq!(body["profiles"][0]["has_api_key"], true);
    assert_eq!(body["profiles"][0]["base_url"], "https://api.example.com");
    assert_eq!(body["profiles"][1]["disabled"], true);
    handle.stop();
}

#[test]
fn write_routes_succeed_with_no_authorization_header() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let (handle, _config_handle) = start_with(config);
    let url = format!("http://{}/api/profiles", handle.addr());
    let response = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"name": "x"}).to_string())
        .expect("no auth gate anymore, this should succeed");
    assert_eq!(response.status().as_u16(), 200);
    handle.stop();
}
