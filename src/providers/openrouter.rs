//! OpenRouter provider — credit stats from `GET /api/v1/key`.
//!
//! Wire shape per <https://openrouter.ai/docs/api_reference/limits>. All
//! figures are USD credits. `limit` / `limit_remaining` are `null` for an
//! unlimited key. The `/api/v1/credits` endpoint is deliberately not used: it
//! requires a management key, which a regular api key is not (403).
//!
//! `label` and the deprecated `rate_limit` object are deliberately not
//! modeled — nothing renders them.

use serde::Deserialize;

use super::{
    DEEPSEEK_BALANCE_ROW_LABEL, StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats,
    url_matches_host,
};

pub(super) const DISPLAY_NAME: &str = "OpenRouter";

pub(super) const ORIGIN: &str = "https://openrouter.ai";

const KEY_PATH: &str = "/api/v1/key";

/// Where an operator mints the api key this provider authenticates with, as
/// published by <https://openrouter.ai/docs/quickstart> ("Your first request").
pub(super) const CONSOLE_URL: &str = "https://openrouter.ai/settings/keys";

pub(super) fn matches_base_url(url: &str) -> bool {
    url_matches_host(url, ORIGIN)
}

pub(super) fn fetch(api_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    let text = super::get_json(&format!("{ORIGIN}{KEY_PATH}"), api_key)?;
    let raw: KeyEnvelope = serde_json::from_str(&text).map_err(|_| ThirdPartyError::Parse)?;
    Ok(stats(&raw.data))
}

/// Pure response → display rows, separated from HTTP for testability.
fn stats(d: &KeyData) -> ThirdPartyStats {
    let mut rows = vec![StatRow {
        label: "credits".to_string(),
        value: String::new(),
        kind: StatRowKind::Heading,
    }];
    // The remaining-credits row shares the wallet label on purpose: the MCP
    // roster's balance rank and the overview's balance column both single that
    // label out, and a remaining credit pool is the spendable balance.
    rows.push(StatRow {
        label: DEEPSEEK_BALANCE_ROW_LABEL.to_string(),
        value: d.limit_remaining.map(dollars).unwrap_or_else(unlimited),
        // Danger must agree with what the row SAYS: anything under half a cent
        // (and any negative answer) renders as `0.00 USD`, so an exact
        // `== 0.0` test would leave a spent key reading as a healthy one.
        kind: if d.limit_remaining.is_some_and(|r| r < 0.005) {
            StatRowKind::Danger
        } else {
            StatRowKind::Body
        },
    });
    rows.push(StatRow {
        label: "used".to_string(),
        value: dollars(d.usage),
        kind: StatRowKind::Body,
    });
    rows.push(StatRow {
        label: "limit".to_string(),
        value: d.limit.map(dollars).unwrap_or_else(unlimited),
        kind: StatRowKind::Body,
    });
    rows.push(StatRow {
        label: "today".to_string(),
        value: dollars(d.usage_daily),
        kind: StatRowKind::Body,
    });
    rows.push(StatRow {
        label: "this week".to_string(),
        value: dollars(d.usage_weekly),
        kind: StatRowKind::Body,
    });
    rows.push(StatRow {
        label: "this month".to_string(),
        value: dollars(d.usage_monthly),
        kind: StatRowKind::Body,
    });
    if d.is_free_tier {
        rows.push(StatRow {
            label: "free tier".to_string(),
            value: String::new(),
            kind: StatRowKind::Faint,
        });
    }
    ThirdPartyStats::from_rows(rows)
}

/// `1.5` → `"1.50 USD"` — the `amount currency` shape the roster's
/// `parse_balance` reads (a `$` prefix would parse as no wallet).
fn dollars(n: f64) -> String {
    format!("{n:.2} USD")
}

fn unlimited() -> String {
    "unlimited".to_string()
}

// ── Wire types ──────────────────────────────────────────────────────────────────

/// `data` is required — an error envelope carrying no key info must never read
/// as usable usage — but its fields default: a degraded body renders zeros
/// rather than dropping the whole wallet, matching z.ai's `Default`-bound
/// envelope. The wire is documented and current, so the leniency is deliberate
/// rather than a shape guess.
#[derive(Debug, Clone, Deserialize)]
struct KeyEnvelope {
    data: KeyData,
}

#[derive(Debug, Clone, Deserialize)]
struct KeyData {
    /// Credit limit for the key, `null` when unlimited.
    #[serde(default)]
    limit: Option<f64>,
    /// Remaining credits, `null` when unlimited.
    #[serde(default)]
    limit_remaining: Option<f64>,
    /// Total credits used, all time.
    #[serde(default)]
    usage: f64,
    /// Credits used in the current UTC day.
    #[serde(default)]
    usage_daily: f64,
    /// Credits used in the current UTC week.
    #[serde(default)]
    usage_weekly: f64,
    /// Credits used in the current UTC month.
    #[serde(default)]
    usage_monthly: f64,
    /// Whether the user has ever paid for credits.
    #[serde(default)]
    is_free_tier: bool,
}

#[cfg(test)]
#[path = "../../tests/inline/providers_openrouter.rs"]
mod tests;
