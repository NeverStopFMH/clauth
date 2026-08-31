//! End-to-end tests for `PATCH /api/config`, driving a real spawned server
//! against a sandboxed home.

use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, AppState, ClockFormat, ConfigHandle, ResetDisplay, ThemeName};
use crate::testutil::HomeSandbox;

fn start_with(config: AppConfig) -> (crate::web::Handle, ConfigHandle) {
    let handle_config: ConfigHandle = Arc::new(RankedMutex::new(config));
    let server =
        crate::web::spawn(Arc::clone(&handle_config), "127.0.0.1:0").expect("server binds");
    (server, handle_config)
}

fn blank_config() -> AppConfig {
    AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    }
}

#[test]
fn patch_applies_only_the_fields_present() {
    let _home = HomeSandbox::new();
    let (handle, config_handle) = start_with(blank_config());
    let url = format!("http://{}/api/config", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(
            serde_json::json!({
                "theme": "compatible",
                "clock_format": "12h",
                "burn_aware_switching": true,
            })
            .to_string(),
        )
        .expect("patch request");
    assert_eq!(response.status().as_u16(), 200);

    #[allow(clippy::unwrap_used, reason = "test-only")]
    let cfg = config_handle.lock().unwrap();
    assert_eq!(cfg.state.theme, Some(ThemeName::Compatible));
    assert_eq!(cfg.state.clock_format, Some(ClockFormat::H12));
    assert!(cfg.state.burn_aware_switching);
    // Untouched fields keep their defaults.
    assert_eq!(cfg.state.reset_display, None);
    assert!(!cfg.state.spend_budget_switching);
    drop(cfg);
    handle.stop();
}

#[test]
fn patch_persists_across_a_config_reload() {
    let _home = HomeSandbox::new();
    let (handle, _config_handle) = start_with(blank_config());
    let url = format!("http://{}/api/config", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"reset_display": "both"}).to_string())
        .expect("patch request");
    assert_eq!(response.status().as_u16(), 200);
    handle.stop();

    let reloaded = crate::profile::load_config().expect("reload state");
    assert_eq!(reloaded.state.reset_display, Some(ResetDisplay::Both));
}

#[test]
fn patch_with_no_authorization_header_still_succeeds() {
    let _home = HomeSandbox::new();
    let (handle, _config_handle) = start_with(blank_config());
    let url = format!("http://{}/api/config", handle.addr());
    let response = ureq::patch(&url)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"theme": "full"}).to_string())
        .expect("no auth gate anymore, this should succeed");
    assert_eq!(response.status().as_u16(), 200);
    handle.stop();
}
