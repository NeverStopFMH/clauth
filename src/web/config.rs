//! Config tab write endpoint — a thin wrapper over `actions::apply_config_patch`.

use tiny_http::StatusCode;

use super::{RouteResult, error_body, ok_body, read_json_body};
use crate::actions::ConfigPatch;
use crate::profile::ConfigHandle;

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
