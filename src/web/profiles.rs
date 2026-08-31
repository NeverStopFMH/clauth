//! Overview + Setup tab write endpoints: thin HTTP wrappers over the
//! existing `actions.rs` business logic — no mutation logic lives here,
//! only request parsing and response shaping.

use std::collections::BTreeMap;

use serde::Deserialize;
use tiny_http::StatusCode;

use super::{RouteResult, error_body, ok_body, read_json_body};
use crate::profile::{ConfigHandle, ProfileName};

#[derive(Deserialize)]
struct SwitchRequest {
    name: String,
}

pub(super) fn switch(config: &ConfigHandle, request: &mut tiny_http::Request) -> RouteResult {
    let body: SwitchRequest = read_json_body(request)?;
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");
    crate::actions::switch_profile(&mut cfg, &ProfileName::from(body.name))
        .map(|()| (StatusCode(200), ok_body()))
        .map_err(|e| (StatusCode(422), error_body(&e.to_string())))
}

#[derive(Deserialize)]
struct ReorderRequest {
    from: usize,
    to: usize,
}

pub(super) fn reorder(config: &ConfigHandle, request: &mut tiny_http::Request) -> RouteResult {
    let body: ReorderRequest = read_json_body(request)?;
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");
    crate::actions::reorder_profile(&mut cfg, body.from, body.to)
        .map(|()| (StatusCode(200), ok_body()))
        .map_err(|e| (StatusCode(422), error_body(&e.to_string())))
}

#[derive(Deserialize)]
struct CreateRequest {
    name: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

pub(super) fn create(config: &ConfigHandle, request: &mut tiny_http::Request) -> RouteResult {
    let body: CreateRequest = read_json_body(request)?;
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");
    let existing: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
    if let Err(e) = crate::actions::validate_profile_name(&body.name, &existing, None) {
        return Err((StatusCode(422), error_body(&e.to_string())));
    }
    crate::actions::create_blank_profile(
        &mut cfg,
        body.name,
        body.base_url,
        body.api_key,
        body.model,
    )
    .map(|()| (StatusCode(200), ok_body()))
    .map_err(|e| (StatusCode(422), error_body(&e.to_string())))
}

/// `DELETE /api/profiles/{name}` — `?force=true` mirrors the CLI's `--force`
/// (override the live-session guard). `url` is the full request target
/// (path + query) so the query string survives past the router's path-only
/// match.
pub(super) fn delete(config: &ConfigHandle, name: &str, url: &str) -> RouteResult {
    let force = url
        .split_once('?')
        .is_some_and(|(_, query)| query.split('&').any(|kv| kv == "force=true"));
    let name = ProfileName::from(name.to_string());
    let rotation = match crate::actions::rotation_guard_for_mutation(&name) {
        Ok(guard) => guard,
        Err(e) => return Err((StatusCode(423), error_body(&e.to_string()))),
    };
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");
    crate::actions::delete_profile(&mut cfg, &name, force, &rotation)
        .map(|()| (StatusCode(200), ok_body()))
        .map_err(|e| (StatusCode(422), error_body(&e.to_string())))
}

/// `PATCH /api/profiles/{name}` — a partial patch: only the top-level fields
/// present in the body are applied, each dispatched to the `actions.rs`
/// function that owns it. `endpoint` carries `base_url` + `api_key` TOGETHER
/// (mirroring `edit_profile_endpoint`'s own signature) rather than as two
/// independent optional fields, so there is no ambiguity between "leave this
/// half alone" and "clear it" — editing the endpoint always sends the whole
/// pair.
#[derive(Deserialize, Default)]
struct PatchRequest {
    #[serde(default)]
    endpoint: Option<EndpointPatch>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    disabled: Option<bool>,
}

#[derive(Deserialize)]
struct EndpointPatch {
    base_url: Option<String>,
    api_key: Option<String>,
}

pub(super) fn patch(
    config: &ConfigHandle,
    name: &str,
    request: &mut tiny_http::Request,
) -> RouteResult {
    let body: PatchRequest = read_json_body(request)?;
    let name = ProfileName::from(name.to_string());
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut cfg = config.lock().expect("config mutex poisoned");

    if let Some(endpoint) = body.endpoint {
        crate::actions::edit_profile_endpoint(&mut cfg, &name, endpoint.base_url, endpoint.api_key)
            .map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
    }

    if let Some(env) = body.env {
        crate::actions::edit_profile_env(&mut cfg, &name, env)
            .map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
    }

    if let Some(disabled) = body.disabled {
        let result = if disabled {
            crate::actions::disable_profile(&mut cfg, &name)
        } else {
            crate::actions::enable_profile(&mut cfg, &name)
        };
        result.map_err(|e| (StatusCode(422), error_body(&e.to_string())))?;
    }

    Ok((StatusCode(200), ok_body()))
}

#[cfg(test)]
#[path = "../../tests/inline/web_profiles.rs"]
mod tests;
