//! Inline tests for the OpenRouter provider — base-URL matching and the
//! `/api/v1/key` response → display-rows mapping.

use super::*;

use crate::providers::Provider;

// ── Provider::from_base_url dispatch ───────────────────────────────────────────
//
// Asserted through the dispatch, not the module fn: the module fn passing while
// the `from_base_url` arm is missing would silently route an OpenRouter profile
// through the generic scanner, and a mutation that drops the arm must red here.

#[test]
fn from_base_url_dispatches_openrouter() {
    for url in [
        "https://openrouter.ai",
        "https://openrouter.ai/api",
        "https://openrouter.ai/api/v1",
        "https://openrouter.ai/api/v1/chat/completions",
        // Hosts are case-insensitive (RFC 3986).
        "https://OPENROUTER.AI/api",
        // An explicit port is still the provider.
        "https://openrouter.ai:443/api",
    ] {
        assert_eq!(
            Provider::from_base_url(url),
            Some(Provider::OpenRouter),
            "{url}"
        );
    }
}

#[test]
fn from_base_url_rejects_host_extension_and_userinfo() {
    // A bare prefix match would claim these and send the profile's API key to
    // the real provider endpoint.
    assert_eq!(
        Provider::from_base_url("https://openrouter.ai.evil.tld"),
        None
    );
    // Everything before an `@` is userinfo, so this host is `evil.tld`.
    assert_eq!(
        Provider::from_base_url("https://openrouter.ai:443@evil.tld"),
        None
    );
    assert_eq!(Provider::from_base_url("http://openrouter.ai"), None);
    assert_eq!(Provider::from_base_url("https://api.anthropic.com"), None);
}

// ── wire parsing ───────────────────────────────────────────────────────────────

#[test]
fn key_response_parses_wire_shape() {
    // Shape per https://openrouter.ai/docs/api_reference/limits
    let json = r#"{
        "data": {
            "label": "my-key",
            "limit": 100.0,
            "limit_remaining": 50.0,
            "usage": 50.0,
            "usage_daily": 10.0,
            "usage_weekly": 20.0,
            "usage_monthly": 50.0,
            "is_free_tier": false
        }
    }"#;
    let raw: KeyEnvelope = serde_json::from_str(json).expect("parse key response");
    assert_eq!(raw.data.limit, Some(100.0));
    assert_eq!(raw.data.limit_remaining, Some(50.0));
    assert_eq!(raw.data.usage, 50.0);
    // Every field the mapping renders, so a serde rename or typo reds here
    // rather than silently zeroing a row.
    assert_eq!(raw.data.usage_daily, 10.0);
    assert_eq!(raw.data.usage_weekly, 20.0);
    assert_eq!(raw.data.usage_monthly, 50.0);
    assert!(!raw.data.is_free_tier);
}

#[test]
fn key_response_without_data_fails_to_parse() {
    // `data` is required: an error envelope carrying no key info must never
    // count as usable usage.
    assert!(serde_json::from_str::<KeyEnvelope>("{}").is_err());
}

// ── response → rows ────────────────────────────────────────────────────────────

#[test]
fn stats_builds_wallet_rows() {
    let raw = KeyData {
        limit: Some(100.0),
        limit_remaining: Some(50.0),
        usage: 50.0,
        usage_daily: 10.0,
        usage_weekly: 20.0,
        usage_monthly: 50.0,
        is_free_tier: false,
    };
    let stats = stats(&raw);
    assert!(stats.is_available);
    assert_eq!(stats.rows.len(), 7);
    assert_eq!(stats.rows[0].kind, StatRowKind::Heading);
    assert_eq!(stats.rows[0].label, "credits");
    // The literal, not the constant: this row's label is a cross-module contract
    // (the MCP roster's wallet rank matches on it), so a rename has to red here
    // rather than follow silently.
    assert_eq!(stats.rows[1].label, "api balance");
    assert_eq!(stats.rows[1].value, "50.00 USD");
    assert_eq!(stats.rows[1].kind, StatRowKind::Body);
    let labels: Vec<&str> = stats.rows[2..].iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        ["used", "limit", "today", "this week", "this month"]
    );
    assert_eq!(stats.rows[2].value, "50.00 USD");
    assert_eq!(stats.rows[3].value, "100.00 USD");
    assert_eq!(stats.rows[4].value, "10.00 USD");
    assert_eq!(stats.rows[5].value, "20.00 USD");
    assert_eq!(stats.rows[6].value, "50.00 USD");
}

#[test]
fn unlimited_limit_reads_unlimited() {
    let raw = KeyData {
        limit: None,
        limit_remaining: None,
        usage: 50.0,
        usage_daily: 0.0,
        usage_weekly: 0.0,
        usage_monthly: 0.0,
        is_free_tier: false,
    };
    let stats = stats(&raw);
    assert_eq!(stats.rows[1].value, "unlimited");
    assert_eq!(stats.rows[1].kind, StatRowKind::Body);
    assert_eq!(stats.rows[3].value, "unlimited");
}

#[test]
fn zero_balance_reads_danger() {
    // A sub-cent remainder formats as `0.00 USD` too, so the Danger predicate
    // must reach it — `== 0.0` would leave a spent key reading as a healthy
    // one. A full cent still formats as `0.01 USD` and stays Body.
    for remaining in [0.0, 0.0005] {
        let raw = KeyData {
            limit: Some(100.0),
            limit_remaining: Some(remaining),
            usage: 100.0,
            usage_daily: 0.0,
            usage_weekly: 0.0,
            usage_monthly: 0.0,
            is_free_tier: false,
        };
        let stats = stats(&raw);
        assert_eq!(
            stats.rows[1].kind,
            StatRowKind::Danger,
            "remaining {remaining}"
        );
        assert_eq!(stats.rows[1].value, "0.00 USD");
    }
    let cent = KeyData {
        limit: Some(100.0),
        limit_remaining: Some(0.01),
        usage: 99.99,
        usage_daily: 0.0,
        usage_weekly: 0.0,
        usage_monthly: 0.0,
        is_free_tier: false,
    };
    let stats = stats(&cent);
    assert_eq!(stats.rows[1].kind, StatRowKind::Body);
    assert_eq!(stats.rows[1].value, "0.01 USD");
}

#[test]
fn free_tier_appends_faint_row() {
    let raw = KeyData {
        limit: Some(1.0),
        limit_remaining: Some(0.5),
        usage: 0.5,
        usage_daily: 0.0,
        usage_weekly: 0.0,
        usage_monthly: 0.0,
        is_free_tier: true,
    };
    let stats = stats(&raw);
    assert_eq!(stats.rows.last().unwrap().label, "free tier");
    assert_eq!(stats.rows.last().unwrap().kind, StatRowKind::Faint);
}
