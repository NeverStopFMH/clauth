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
    let m = refresh_transient(
        "flaky",
        &Transient::new("could not reach anthropic", Retry::Connection),
    );
    assert_eq!(
        m.line(),
        "could not refresh 'flaky' before switching: could not reach anthropic: check your \
         connection and retry"
    );
    // The head stays fixed-length regardless of the (arbitrary, possibly long)
    // error text, so it can never wrap the toast's bold first line.
    assert_eq!(
        m.toast().lines().next().unwrap(),
        "could not refresh 'flaky' before switching"
    );
    assert_eq!(
        m.toast().lines().nth(1).unwrap(),
        "could not reach anthropic: check your connection and retry"
    );
}

/// The whole reason the kind travels inside the value: `check your connection`
/// is wrong advice for a throttle or a 5xx, and it used to be appended
/// unconditionally to all four `AuthGate::Transient` causes — including the
/// rotation-lock one, which already tells you to retry for a different reason.
#[test]
fn the_retry_hint_follows_the_kind_not_the_call_site() {
    let connection = Transient::new("could not reach anthropic", Retry::Connection);
    assert_eq!(
        connection.text(),
        "could not reach anthropic: check your connection and retry"
    );

    let wait = Transient::with_status("anthropic is throttling requests", 429, Retry::Wait);
    assert_eq!(
        wait.text(),
        "anthropic is throttling requests: retry in a moment"
    );
    assert!(
        !wait.text().contains("connection"),
        "a 429 must never be blamed on the operator's connection: {}",
        wait.text()
    );

    // `Stated` adds nothing: the cause already names its own next step, and a
    // second one contradicts it.
    let stated = Transient::new(
        "'work' rotation lock busy; retry after the in-flight refresh",
        Retry::Stated,
    );
    assert_eq!(
        stated.text(),
        "'work' rotation lock busy; retry after the in-flight refresh"
    );
}

/// The CLI/daemon surfaces name the HTTP status; the toast and MCP forms do not.
/// Asserted together so neither half can drift alone — a status that silently
/// stops reaching stderr looks exactly like one that was never added.
#[test]
fn only_the_status_bearing_form_names_the_status() {
    let t = Transient::with_status("anthropic is having trouble", 503, Retry::Wait);
    assert_eq!(
        t.text_with_status(),
        "anthropic is having trouble (HTTP 503): retry in a moment"
    );
    assert_eq!(t.text(), "anthropic is having trouble: retry in a moment");
    assert!(
        !t.text().contains("503"),
        "the canned form must not leak the status: {}",
        t.text()
    );

    assert_eq!(
        refresh_transient_cli("work", &t).line(),
        "could not refresh 'work' before switching: anthropic is having trouble (HTTP 503): \
         retry in a moment"
    );
    assert!(
        !refresh_transient("work", &t).line().contains("503"),
        "the non-CLI constructor must stay status-free"
    );

    // A failure that never saw a status has nothing honest to add, so the two
    // forms coincide rather than inventing one.
    let no_status = Transient::new("could not reach anthropic", Retry::Connection);
    assert_eq!(no_status.text_with_status(), no_status.text());
}

/// `detail()` is what lets a surface whose own first line states the condition
/// avoid restating it. The fallback arm matters: a detail-less `Message` must
/// still render something, or a caller would mint copy of its own.
#[test]
fn detail_returns_the_next_step_alone_and_falls_back_to_the_head() {
    assert_eq!(
        login_expired("work").detail(),
        "refresh token revoked or invalid: run clauth login work"
    );
    let bare = Message {
        head: "done".to_string(),
        detail: None,
    };
    assert_eq!(bare.detail(), "done");
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
    assert_eq!(plan_label(&canceled), "Claude Free");

    // A genuine, never-subscribed free account looks the same here — the
    // canceled distinction lives on the status line, not the plan label.
    let free = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: None,
    };
    assert_eq!(plan_label(&free), "Claude Free");
}
