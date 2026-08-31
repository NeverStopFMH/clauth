//! `GET /api/tokens` end-to-end: reads `~/.claude/stats-cache.json` verbatim
//! through `crate::tokens::load_base`, so these seed that file directly
//! rather than driving the live transcript-scanning loader.

use crate::testutil::HomeSandbox;

fn empty_config() -> crate::profile::ConfigHandle {
    std::sync::Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn start() -> crate::web::Handle {
    crate::web::spawn(empty_config(), "127.0.0.1:0").expect("server binds")
}

#[test]
fn no_cache_file_is_503() {
    let _home = HomeSandbox::new();
    let handle = start();
    let url = format!("http://{}/api/tokens", handle.addr());
    let err = ureq::get(&url).call().expect_err("no stats-cache.json yet");
    let ureq::Error::StatusCode(503) = err else {
        panic!("expected 503, got {err:?}");
    };
    handle.stop();
}

#[test]
fn reports_lifetime_totals_and_grouped_models() {
    let _home = HomeSandbox::new();
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("create claude dir");
    let cache = serde_json::json!({
        "totalSessions": 12,
        "totalMessages": 340,
        "dailyModelTokens": [
            {"date": "2026-08-30", "tokensByModel": {"claude-opus": 1000}}
        ],
        "modelUsage": {
            "claude-opus": {
                "inputTokens": 500,
                "outputTokens": 500,
                "cacheReadInputTokens": 100,
                "cacheCreationInputTokens": 50
            }
        }
    });
    std::fs::write(claude_dir.join("stats-cache.json"), cache.to_string()).expect("seed cache");

    let handle = start();
    let url = format!("http://{}/api/tokens", handle.addr());
    let mut response = ureq::get(&url).call().expect("tokens request");
    assert_eq!(response.status().as_u16(), 200);
    let text = response.body_mut().read_to_string().expect("body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("json body");
    assert_eq!(body["total_input"], 500);
    assert_eq!(body["total_output"], 500);
    assert_eq!(body["total_sessions"], 12);
    assert_eq!(body["models"][0]["model"], "claude-opus");
    assert_eq!(body["daily"][0]["tokens"], 1000);
    handle.stop();
}
