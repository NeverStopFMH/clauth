//! Config tab endpoints — a read of the ~14 global settings (`GET /api/status`
//! only carries `wrap_off`/`refresh_interval_ms`, not the rest) plus the
//! write endpoint over `actions::apply_config_patch`.

use tiny_http::StatusCode;

use super::{RouteResult, error_body, ok_body, read_json_body};
use crate::actions::ConfigPatch;
use crate::profile::ConfigHandle;

/// `GET /api/config` — every field [`ConfigPatch`] can write, at its
/// currently EFFECTIVE value: fields with a fail-safe default
/// (`reset_display`/`clock_format`/`weekly_switch_threshold`/
/// `burn_switch_floor_pct`/`burn_horizon_cap_ms`) go through their
/// `AppState` accessor rather than the raw `Option`, so a form populated
/// from this response shows the value actually in effect, not `null`. The
/// two real tri-states (`theme` unset = auto-detect, `default_divergence`
/// unset = "ask") are passed through raw.
pub(super) fn get(config: &ConfigHandle) -> RouteResult {
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let cfg = config.lock().expect("config mutex poisoned");
    let state = &cfg.state;
    let body = serde_json::json!({
        "theme": state.theme,
        "reset_display": state.reset_display(),
        "clock_format": state.clock_format(),
        "default_divergence": state.default_divergence,
        "switch_off_when_spent": state.switch_off_when_spent,
        "weekly_switch_threshold": state.weekly_switch_threshold_pct(),
        "refresh_interval_ms": state.refresh_interval_ms,
        "burn_aware_switching": state.burn_aware_switching,
        "burn_switch_floor_pct": state.burn_switch_floor_pct(),
        "burn_horizon_cap_ms": state.burn_horizon_cap_ms(),
        "spend_budget_switching": state.spend_budget_switching,
        "switch_off_when_budget_spent": state.switch_off_when_budget_spent,
        "preemptive_rotation": state.preemptive_rotation,
        "refresh_spent_accounts": state.refresh_spent_accounts,
    });
    Ok((StatusCode(200), body.to_string()))
}

/// `PATCH /api/config` — a partial patch over the ~14 global settings the
/// Config tab exposes (theme, clock notation, scheduler interval, burn-aware
/// auto-switch knobs, …). Only fields present in the body are applied.
pub(super) fn patch(config: &ConfigHandle, request: &mut tiny_http::Request) -> RouteResult {
    let patch: ConfigPatch = read_json_body(request)?;
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");
    crate::actions::apply_config_patch(&mut cfg, patch)
        .map(|()| (StatusCode(200), ok_body()))
        .map_err(|e| (StatusCode(422), error_body(&e.to_string())))
}

#[cfg(test)]
#[path = "../../tests/inline/web_config.rs"]
mod tests;
