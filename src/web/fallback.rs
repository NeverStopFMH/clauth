//! Fallback tab endpoints — a read (the per-member editable state the
//! dashboard's form needs, which `GET /api/status`'s slim `fallback` object
//! doesn't carry) plus the write endpoints extracted from `tui/app.rs`'s own
//! fallback-detail keystrokes.

use serde::Deserialize;
use tiny_http::StatusCode;

use super::{RouteResult, error_body, ok_body, read_json_body};
use crate::fallback::{member_weekly_line, threshold_for};
use crate::profile::{ConfigHandle, ProfileName};
use crate::usage::LABEL_5H;

#[derive(Deserialize)]
struct ChainRequest {
    chain: Vec<String>,
}

/// `GET /api/fallback` — every chain member's editable fallback fields, plus
/// the profiles not yet in the chain (candidates for the "+ add" control).
/// `GET /api/status`'s `profiles[].fallback` only carries `position`/
/// `threshold`/`armed` (the daemon-facing contract documented in
/// `wiki/Daemon.md`); the dashboard's Fallback tab additionally needs
/// `weekly_threshold`/`max_auto_spend`/`preferred`/`last_resort` to populate
/// its form, which are dashboard-only reads with no reason to grow that
/// shared contract.
pub(super) fn list(config: &ConfigHandle) -> RouteResult {
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let cfg = config.lock().expect("config mutex poisoned");
    let chain_soft = cfg.state.weekly_switch_threshold_pct();
    let chain: Vec<serde_json::Value> = cfg
        .state
        .fallback_chain
        .iter()
        .filter_map(|name| cfg.find(name).map(|p| (name, p)))
        .map(|(name, p)| {
            // Read the disk cache (same source `GET /api/status` publishes
            // from), not the in-memory `Profile.usage` field: a daemon that
            // stood down its own usage-fetch lease to another live process
            // (TUI, another `clauth` instance) never populates its own
            // in-memory copy, so reading that field here would show "no
            // data" on Fallback while Overview/Usage — both status.json-
            // sourced — show real numbers from the same account.
            let utilization_5h = crate::profile_json::published_windows(name)
                .into_iter()
                .find(|w| w.label == LABEL_5H)
                .map(|w| w.utilization_pct);
            serde_json::json!({
                "name": name.as_ref(),
                "armed": cfg.is_active(name),
                "utilization_5h": utilization_5h,
                "threshold": threshold_for(p),
                "weekly_threshold": member_weekly_line(p, chain_soft),
                "max_auto_spend": p.max_auto_spend,
                "preferred": p.preferred,
                "last_resort": p.last_resort,
            })
        })
        .collect();
    let candidates: Vec<&str> = cfg
        .profiles
        .iter()
        .map(|p| p.name.as_ref())
        .filter(|name| !cfg.state.fallback_chain.iter().any(|n| n.as_ref() == *name))
        .collect();
    Ok((
        StatusCode(200),
        serde_json::json!({ "chain": chain, "candidates": candidates }).to_string(),
    ))
}

/// `PATCH /api/fallback` — replaces the whole chain membership + order in
/// one write (the dashboard's drag-to-reorder sends the complete new list),
/// unlike the TUI's one-member-at-a-time ⇧↑/⇧↓.
pub(super) fn set_chain(config: &ConfigHandle, request: &mut tiny_http::Request) -> RouteResult {
    let body: ChainRequest = read_json_body(request)?;
    let chain = body.chain.into_iter().map(ProfileName::from).collect();
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");
    crate::actions::set_fallback_chain(&mut cfg, chain)
        .map(|()| (StatusCode(200), ok_body()))
        .map_err(|e| (StatusCode(422), error_body(&e.to_string())))
}

/// `PATCH /api/profiles/{name}/fallback` — only the fields present are
/// applied. `preferred`/`last_resort` are booleans a form checkbox sends
/// (the desired end state), not the TUI's raw "flip it" keystroke — so this
/// only calls the underlying `toggle_*` (which DOES flip, and carries the
/// chain-wide exclusivity logic) when the requested value actually differs
/// from the current one, keeping the endpoint idempotent.
#[derive(Deserialize, Default)]
struct MemberPatch {
    #[serde(default)]
    threshold: Option<f64>,
    /// No explicit-clear-to-default support yet (that needs the
    /// present-vs-null-vs-omitted three-way serde normally gets via a
    /// `deserialize_with` helper) — a future slice's job, not this one's.
    #[serde(default)]
    weekly_threshold: Option<f64>,
    #[serde(default)]
    max_auto_spend: Option<f64>,
    #[serde(default)]
    preferred: Option<bool>,
    #[serde(default)]
    last_resort: Option<bool>,
}

pub(super) fn patch_member(
    config: &ConfigHandle,
    name: &str,
    request: &mut tiny_http::Request,
) -> RouteResult {
    let body: MemberPatch = read_json_body(request)?;
    let name = ProfileName::from(name.to_string());
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");

    if let Some(value) = body.threshold {
        crate::actions::set_fallback_threshold(&mut cfg, &name, value)
            .map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
    }
    if let Some(value) = body.weekly_threshold {
        crate::actions::set_weekly_threshold(&mut cfg, &name, Some(value))
            .map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
    }
    if let Some(value) = body.max_auto_spend {
        crate::actions::set_max_auto_spend(&mut cfg, &name, value)
            .map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
    }
    if let Some(want) = body.preferred {
        let current = cfg
            .find(&name)
            .ok_or_else(|| (StatusCode(404), error_body("profile not found")))?
            .preferred;
        if current != want {
            crate::actions::toggle_preferred(&mut cfg, &name)
                .map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
        }
    }
    if let Some(want) = body.last_resort {
        let current = cfg
            .find(&name)
            .ok_or_else(|| (StatusCode(404), error_body("profile not found")))?
            .last_resort;
        if current != want {
            crate::actions::toggle_last_resort(&mut cfg, &name)
                .map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
        }
    }

    Ok((StatusCode(200), ok_body()))
}

#[cfg(test)]
#[path = "../../tests/inline/web_fallback.rs"]
mod tests;
