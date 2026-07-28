#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

// The centralized diagnostics are load-bearing precisely because they render on
// three surfaces from one definition; these pin that one head reaches all three
// without drifting, and that the CLI/log and toast forms differ only in the
// head↔detail separator.

#[test]
fn login_expired_shares_one_head_across_line_and_toast() {
    let m = login_expired("work");
    assert_eq!(
        m.line(),
        "login for 'work' has expired: refresh token revoked or invalid: run clauth login work"
    );
    assert_eq!(
        m.toast(),
        "login for 'work' has expired\nrefresh token revoked or invalid: run clauth login work"
    );
    // The bold toast head is exactly the line() prefix before the separator.
    assert_eq!(
        m.toast().lines().next().unwrap(),
        "login for 'work' has expired"
    );
}

#[test]
fn refresh_transient_carries_the_error_in_the_detail() {
    let m = refresh_transient("flaky", "no network");
    assert_eq!(
        m.line(),
        "could not refresh 'flaky' before switching: no network: check your connection and retry"
    );
    // The head stays fixed-length regardless of the (arbitrary, possibly long)
    // error text, so it can never wrap the toast's bold first line.
    assert_eq!(
        m.toast().lines().next().unwrap(),
        "could not refresh 'flaky' before switching"
    );
    assert_eq!(
        m.toast().lines().nth(1).unwrap(),
        "no network: check your connection and retry"
    );
}

#[test]
fn line_and_toast_collapse_to_the_head_when_detail_is_absent() {
    let m = Message {
        head: "done".to_string(),
        detail: None,
    };
    assert_eq!(m.line(), "done");
    assert_eq!(m.toast(), "done");
}

#[test]
fn resolve_in_tui_names_the_clauth_surface() {
    assert!(RESOLVE_IN_TUI.contains("clauth TUI"));
}

#[test]
fn format_pct_drops_trailing_zero_on_whole_numbers() {
    assert_eq!(format_pct(42.0), "42%");
    assert_eq!(format_pct(0.0), "0%");
    assert_eq!(format_pct(100.0), "100%");
}

#[test]
fn format_pct_shows_fractional_percent() {
    assert_eq!(format_pct(42.3), "42.3%");
}

#[test]
fn plan_label_renders_the_tier_only_the_canceled_marker_is_on_the_status_line() {
    let canceled = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    };
    assert_eq!(plan_label(&canceled).as_deref(), Some("Claude Free"));

    // A genuine, never-subscribed free account looks the same here — the
    // canceled distinction lives on the status line, not the plan label.
    let free = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: None,
    };
    assert_eq!(plan_label(&free).as_deref(), Some("Claude Free"));
}

/// An unfetched plan has no tier at all. `endpoint_label` says so with `None`
/// so each surface picks its own no-data form — a bare "Claude" here read as a
/// real plan, and shipped to the Overview chip, the Usage `plan` row and
/// `which --json`'s `tier` alike.
#[test]
fn endpoint_label_reports_no_tier_for_an_unfetched_plan() {
    // No credentials at all: nothing on disk claims a tier.
    let bare = crate::testutil::blank_profile("a");
    assert_eq!(endpoint_label(&bare), None);

    // A token whose `subscription_type` is not one clauth classifies is the
    // same "we do not know" — never a fabricated tier.
    let mut unclassified = crate::testutil::blank_profile("b");
    unclassified.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("something_new".into()),
        }),
    });
    assert_eq!(endpoint_label(&unclassified), None);

    // A fetched plan whose tier never classified, with no token claim to fall
    // through to, reads the same way.
    let mut unknown_plan = crate::testutil::blank_profile("c");
    unknown_plan.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Unknown,
            subscription_status: None,
        }),
        ..Default::default()
    });
    assert_eq!(endpoint_label(&unknown_plan), None);
}

/// An UNCLASSIFIED fetched plan is not an answer, so it falls through to the
/// token claim exactly the way `profile_json::tier_label` does. Short-circuiting
/// on it instead left this surface reporting "no data" while `status.json` showed
/// a tier for the same account at the same instant.
#[test]
fn endpoint_label_falls_through_an_unclassified_fetched_plan_to_the_token() {
    let token = |sub: &str| {
        Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at".into(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: Some(sub.into()),
            }),
        })
    };
    let plan = |tier| {
        Some(crate::usage::UsageInfo {
            plan: Some(PlanInfo {
                tier,
                subscription_status: None,
            }),
            ..Default::default()
        })
    };

    let mut unclassified = crate::testutil::blank_profile("a");
    unclassified.usage = plan(PlanTier::Unknown);
    unclassified.credentials = token("max");
    assert_eq!(
        endpoint_label(&unclassified).as_deref(),
        Some("Claude Max"),
        "an unclassified fetched tier must not mask the token's claim"
    );

    // The other arm of the same branch: a fetched tier that DID classify still
    // wins over a disagreeing token, so the fall-through cannot invert priority.
    let mut disagreeing = crate::testutil::blank_profile("b");
    disagreeing.usage = plan(PlanTier::Max(Some(20)));
    disagreeing.credentials = token("pro");
    assert_eq!(
        endpoint_label(&disagreeing).as_deref(),
        Some("Claude Max 20x"),
        "the fetched tier is the better source and still wins"
    );
}

/// A `Free` login round-trips end to end: `login_profile_from_raw` stores
/// `"free"`, and this surface reads it back as the plan rather than the no-data
/// form. Free has no `has_claude_*` flag to recover it, so the token is the only
/// pre-fetch source it has.
#[test]
fn endpoint_label_reads_back_a_free_logins_stored_token() {
    let mut free = crate::testutil::blank_profile("a");
    free.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("free".into()),
        }),
    });
    assert_eq!(endpoint_label(&free).as_deref(), Some("Claude Free"));
}

/// The other direction, all three branches: a real tier still renders, `Free`
/// is untouched by the unfetched-plan change, and a third-party profile still
/// gets its raw endpoint url back.
#[test]
fn endpoint_label_still_renders_every_known_tier_and_the_endpoint_url() {
    let mut fetched = crate::testutil::blank_profile("a");
    fetched.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(20)),
            subscription_status: None,
        }),
        ..Default::default()
    });
    assert_eq!(endpoint_label(&fetched).as_deref(), Some("Claude Max 20x"));

    let mut free = crate::testutil::blank_profile("b");
    free.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: None,
        }),
        ..Default::default()
    });
    assert_eq!(endpoint_label(&free).as_deref(), Some("Claude Free"));

    let mut token_only = crate::testutil::blank_profile("c");
    token_only.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("pro".into()),
        }),
    });
    assert_eq!(endpoint_label(&token_only).as_deref(), Some("Claude Pro"));

    let mut third_party = crate::testutil::blank_profile("d");
    third_party.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    assert_eq!(
        endpoint_label(&third_party).as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "the base-url branch must keep returning the raw endpoint"
    );
}
