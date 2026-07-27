#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `clauth list` table renderer (`render_table`): hide/reveal of disabled
//! profiles, the active marker, and exact column layout. Driven over the real
//! `build_status` body under a `HomeSandbox`, the same data path
//! `clauth status --json` reads, so a drift in either surface reds here.

use super::*;

use crate::profile::{AppState, ClaudeCredentials, OAuthToken, Profile};
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::testutil::HomeSandbox;
use crate::usage::{PlanInfo, PlanTier, UsageInfo, UsageWindow};

fn oauth(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".to_string()),
        }),
    });
    p
}

/// Warm `name`'s OAuth usage cache: a `Max 5x` plan and fixed 5h/7d utilization
/// so the rounding and the plan label are pinned, not incidental.
fn warm_usage(name: &str, five_h: f64, seven_d: f64) {
    write_profile_cache(
        name,
        USAGE_CACHE_FILE,
        &UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Max(Some(5)),
                subscription_status: None,
            }),
            five_hour: Some(UsageWindow {
                utilization: five_h,
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: seven_d,
                resets_at: None,
            }),
            ..Default::default()
        },
    );
}

const HEADER: &str = "  PROFILE  PLAN       5H     7D  ENDPOINT";
// 42.4 → 42.4%, 17.6 → 17.6%: format_pct drops only trailing `.0`.
const WORK_ROW: &str = "* work     Max 5x  42.4%  17.6%  -";

#[test]
fn list_table_hides_disabled_by_default_and_marks_the_active_profile() {
    let _home = HomeSandbox::new();
    let mut off = oauth("off");
    off.disabled = true;
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work"), off],
    };
    config.state.active_profile = Some("work".into());
    warm_usage("work", 42.4, 17.6);

    let body = build_status(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &body);

    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [HEADER, WORK_ROW],
        "only the active profile is shown"
    );
    assert!(
        !table.contains("off"),
        "a disabled profile must not appear without --all/--disabled"
    );
}

#[test]
fn list_table_reveals_disabled_with_a_trailing_marker_when_included() {
    let _home = HomeSandbox::new();
    let mut off = oauth("off");
    off.disabled = true;
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work"), off],
    };
    config.state.active_profile = Some("work".into());
    warm_usage("work", 42.4, 17.6);

    let body = build_status(&config, config.state.refresh_interval_ms, None, true);
    let table = render_table(&config, &body);

    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            HEADER,
            WORK_ROW,
            "  off      Max         -      -  - (disabled)",
        ],
        "the disabled row keeps its columns aligned and carries the (disabled) marker"
    );
}

#[test]
fn list_table_shows_provider_as_plan_and_the_base_url_endpoint_for_a_third_party() {
    let _home = HomeSandbox::new();
    let mut zai = Profile::new(
        "z.ai".to_string(),
        Some("https://api.z.ai/api/anthropic".to_string()),
        Some("sk-test".to_string()),
    );
    zai.provider = crate::providers::Provider::from_base_url("https://api.z.ai/api/anthropic");
    assert!(
        zai.is_third_party(),
        "fixture must be a third-party account"
    );
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![zai],
    };

    let body = build_status(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &body);

    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            "  PROFILE  PLAN  5H  7D  ENDPOINT",
            "  z.ai     Z.ai   -   -  https://api.z.ai/api/anthropic",
        ],
        "a third-party account shows its provider as the plan and its base url as the endpoint"
    );
}

#[test]
fn list_table_reports_no_accounts_when_empty() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };
    let body = build_status(&config, config.state.refresh_interval_ms, None, true);
    assert_eq!(
        render_table(&config, &body),
        "no accounts yet. add one with `clauth login <name>`.\n"
    );
}
