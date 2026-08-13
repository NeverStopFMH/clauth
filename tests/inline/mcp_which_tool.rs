#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Guard coverage for the MCP `which` tool's own `tier` field.
//!
//! It resolves through `profile_json::tier_label` — the same helper
//! `clauth which --json` and `status.json` call — so a canceled account reports
//! the plan its `/profile` cache holds rather than the one its login token still
//! claims. Nothing else in the suite drives this tool, so without this the field
//! is free to return anything at all and every other surface's tier pin stays
//! green.

use super::*;

use crate::profile::{
    AppState, ClaudeCredentials, OAuthToken, Profile, save_app_state, save_profile,
};
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::testutil::{ConfigDirSandbox, HomeSandbox};
use crate::usage::{PlanInfo, PlanTier, UsageInfo};

/// Seed one account in the canceled-after-login shape: its stored token still
/// claims `pro` (written once at login, never refreshed) while its cached
/// `/profile` plan has moved to `Free`.
fn seed_canceled_account() {
    let mut profile = Profile::new("kerry".to_string(), None, None);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-kerry".to_string(),
            refresh_token: Some("rt-kerry".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: Some("pro".to_string()),
        }),
    });
    save_profile(&profile).expect("save profile");
    save_app_state(&AppState {
        active_profile: Some("kerry".into()),
        profiles: vec!["kerry".into()],
        ..Default::default()
    })
    .expect("save state");

    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    };
    write_profile_cache("kerry", USAGE_CACHE_FILE, &usage);
}

/// Drive the async `which` tool on a current-thread runtime, mirroring
/// `call_switch` in the switch-tool suite.
fn call_which() -> CallToolResult {
    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .which(Parameters(WhichArgs {
                format: Some("json".to_string()),
            }))
            .await
    })
    .expect("which returns a tool result, never a transport error")
}

/// The tool's payload is the first content block; the second is the usage footer.
fn which_payload(result: &CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("which payload text");
    serde_json::from_str(&text).expect("parse which payload")
}

#[test]
fn which_tool_reports_the_cached_tier_not_the_login_claim() {
    let home = HomeSandbox::new();
    seed_canceled_account();
    // Resolve by runtime dir rather than by loaded credentials: the `session_dir`
    // tier attributes the session from the path alone, so the fixture does not
    // depend on whatever `~/.claude` holds. The `<pid>-<seq>` shape is load
    // bearing — `is_session_id` rejects anything else and the session would fall
    // through unresolved.
    let runtime = home.home().join(".clauth/profiles/kerry/runtime-4242-1");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let _dir = ConfigDirSandbox::new(&home, &runtime);

    let payload = which_payload(&call_which());

    assert_eq!(
        payload["profile"], "kerry",
        "fixture control: the session resolved to the seeded account"
    );
    assert_eq!(
        payload["source"], "session_dir",
        "fixture control: resolved by runtime dir, not by ambient credentials"
    );
    assert_eq!(payload["tier"], "Free");
}
