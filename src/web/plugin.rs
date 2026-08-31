//! Plugin tab endpoints: a read-only registration probe, and the two
//! actions the TUI's Plugin tab can trigger (`f` install, self-heal).

use tiny_http::StatusCode;

use super::{error_body, ok_body};

/// `GET /api/plugin/status` — the same read-only probes the TUI's Plugin tab
/// renders from, summarized: whether Claude Code has the plugin installed,
/// whether the marketplace entry exists, how (if at all) the MCP server is
/// wired, and the detected `claude` CLI version.
pub(super) fn status() -> (StatusCode, String) {
    let records = crate::plugin_probe::installed_records();
    let marketplace = crate::plugin_probe::marketplace_known();
    let mcp_wiring = crate::plugin_probe::manual_mcp_wiring();
    let body = serde_json::json!({
        "installed": !records.is_empty(),
        "install_records": records.iter().map(|r| serde_json::json!({
            "scope": r.scope,
            "version": r.version,
            "installed_at": r.installed_at,
        })).collect::<Vec<_>>(),
        "marketplace_known": marketplace.is_some(),
        "marketplace_repo": marketplace.and_then(|m| m.repo),
        "mcp_wiring": match mcp_wiring {
            crate::plugin_probe::McpWiring::GlobalConfig => "global_config",
            crate::plugin_probe::McpWiring::ProjectFile => "project_file",
            crate::plugin_probe::McpWiring::None => "none",
        },
        "claude_version": crate::plugin_probe::cc_version(),
    });
    (StatusCode(200), body.to_string())
}

/// `POST /api/plugin/install` — drives `agentgear`'s installer the same way
/// the TUI's Plugin tab `f` key does.
pub(super) fn install() -> (StatusCode, String) {
    match crate::plugin_host::install() {
        Ok(outcome) => (
            StatusCode(200),
            serde_json::json!({"ok": true, "outcome": outcome.to_string()}).to_string(),
        ),
        Err(e) => (StatusCode(422), error_body(&e.to_string())),
    }
}

/// `POST /api/plugin/self-heal` — repairs a broken registration; a no-op
/// (still 200) when nothing was broken.
pub(super) fn self_heal() -> (StatusCode, String) {
    match crate::plugin_host::self_heal() {
        Ok(()) => (StatusCode(200), ok_body()),
        Err(e) => (StatusCode(422), error_body(&e.to_string())),
    }
}

#[cfg(test)]
#[path = "../../tests/inline/web_plugin.rs"]
mod tests;
