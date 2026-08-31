//! Fallback tab write endpoints — thin wrappers over the `actions.rs`
//! functions extracted from `tui/app.rs`'s own fallback-detail keystrokes.

use serde::Deserialize;
use tiny_http::StatusCode;

use super::{RouteResult, error_body, ok_body, read_json_body};
use crate::profile::{ConfigHandle, ProfileName};

#[derive(Deserialize)]
struct ChainRequest {
    chain: Vec<String>,
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
