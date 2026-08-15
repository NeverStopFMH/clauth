#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::profile_cache::write_profile_cache;
use crate::testutil::{HomeSandbox, blank_profile};
use crate::usage::{PlanInfo, PlanTier};

/// `tier_label` feeds the MCP `profiles` rows (roster and session scope), and
/// reads straight off `usage_cache.json` — never a live fetch. A canceled
/// subscription reports its TIER here like every other account: the org drops to
/// `claude_free` on cancellation, so `Free` already carries the fact, and the
/// canceled marker belongs on the status line (the `⊖` pill), not in a field
/// every other path fills with a tier.
#[test]
fn tier_label_reports_the_tier_of_a_canceled_account() {
    let _home = HomeSandbox::new();
    let profile = blank_profile("kerry");
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    };
    write_profile_cache("kerry", USAGE_CACHE_FILE, &usage);

    assert_eq!(tier_label(&profile), Some("Free".to_string()));
}

/// Code invariant, not a claim about any observed account: whatever tier the
/// cache holds is what this reports, `subscription_status` notwithstanding. A
/// paid tier is the fixture that can tell the two apart — `Free` alone cannot
/// prove the status was not substituted, since the canceled arm returned a
/// different string but the free one returns the same tier either way.
#[test]
fn tier_label_never_substitutes_canceled_for_a_paid_tier() {
    let _home = HomeSandbox::new();
    let profile = blank_profile("kerry");
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(20)),
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    };
    write_profile_cache("kerry", USAGE_CACHE_FILE, &usage);

    assert_eq!(tier_label(&profile), Some("Max 20x".to_string()));
}

/// Regression guard the other direction: an un-canceled cached plan still
/// reports its real tier, not a false "canceled".
#[test]
fn tier_label_reports_the_real_tier_when_not_canceled() {
    let _home = HomeSandbox::new();
    let profile = blank_profile("kerry");
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(5)),
            subscription_status: None,
        }),
        ..Default::default()
    };
    write_profile_cache("kerry", USAGE_CACHE_FILE, &usage);

    assert_eq!(tier_label(&profile), Some("Max 5x".to_string()));
}
