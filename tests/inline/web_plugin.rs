//! `GET /api/plugin/status` end-to-end (pure reads, safe to run anywhere).
//!
//! `install`/`self-heal` are deliberately NOT exercised here: both shell out
//! to the real `claude` binary via `agentgear`, and the sandboxed test
//! harness for that (`testutil::FakeClaude`) is Unix-only (a PATH-shimmed
//! shell script), so a cross-platform HTTP-level test would either skip on
//! Windows or risk touching a real Claude Code installation. Covered instead
//! by `plugin_host`'s own existing `FakeClaude`-backed tests; this endpoint
//! is a one-line wrapper over the same functions those already exercise.

use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, AppState, ConfigHandle};
use crate::testutil::HomeSandbox;

const TEST_TOKEN: &str = "test-token-0123456789";

fn start() -> (crate::web::Handle, ConfigHandle) {
    let config: ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    }));
    let server = crate::web::spawn(Arc::clone(&config), TEST_TOKEN.to_string(), "127.0.0.1:0")
        .expect("server binds");
    (server, config)
}

#[test]
fn status_is_open_and_reports_not_installed_on_a_fresh_sandbox() {
    let _home = HomeSandbox::new();
    let (handle, _config) = start();
    let url = format!("http://{}/api/plugin/status", handle.addr());
    let mut response = ureq::get(&url).call().expect("status request");
    assert_eq!(response.status().as_u16(), 200);
    let text = response.body_mut().read_to_string().expect("body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("json body");
    assert_eq!(body["installed"], false);
    assert_eq!(body["marketplace_known"], false);
    assert_eq!(body["mcp_wiring"], "none");
    handle.stop();
}
