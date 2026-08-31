//! Tokens tab read endpoint — an MVP subset of the TUI's Tokens dashboard
//! (`src/tui/render/tokens.rs`), which additionally renders a daily-trend
//! sparkline, an hour-of-day chart, a period filter (daily/weekly/monthly),
//! and a per-model drill-down. Reproducing all of that is future work once a
//! frontend actually needs it; this first pass exposes lifetime totals, the
//! grouped model breakdown, and the raw daily trend, which covers "how much
//! have I used" at a glance.
//!
//! Deliberately calls [`crate::tokens::load_base`], not the TUI's live
//! background loader (`crate::tokens::spawn`, which does an initial
//! `load_base` for instant paint and then a transcript sweep for today's
//! activity): `load_base` reads only the persisted `stats-cache.json` (sub-
//! millisecond, safe to call inline on this server's single request-handling
//! thread), while the live loader is a stateful background pipeline built to
//! feed the TUI's persistent `App`, not a stateless HTTP GET. The trade-off:
//! `today`'s in-flight activity (not yet folded into the on-disk cache) is
//! absent here — everything else is the same on-disk source the TUI itself
//! reads first before its transcript top-up arrives.

use tiny_http::StatusCode;

use super::{RouteResult, error_body};

/// `GET /api/tokens`.
pub(super) fn get() -> RouteResult {
    let Ok(claude_dir) = crate::profile::claude_dir() else {
        return Ok((
            StatusCode(503),
            error_body("could not resolve the Claude Code config dir"),
        ));
    };
    let Some(stats) = crate::tokens::load_base(&claude_dir) else {
        return Ok((StatusCode(503), error_body("no token stats cached yet")));
    };

    let models: Vec<serde_json::Value> = crate::tokens::group_models(&stats.models)
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "model": m.model,
                "input": m.input,
                "output": m.output,
                "cache_read": m.cache_read,
                "cache_create": m.cache_create,
            })
        })
        .collect();
    let daily: Vec<serde_json::Value> = stats
        .daily
        .iter()
        .map(|d| serde_json::json!({ "date": d.date, "tokens": d.tokens }))
        .collect();

    let body = serde_json::json!({
        "total_input": stats.total_input,
        "total_output": stats.total_output,
        "total_cache_read": stats.total_cache_read,
        "total_cache_create": stats.total_cache_create,
        "total_sessions": stats.total_sessions,
        "total_messages": stats.total_messages,
        "cache_hit_ratio": stats.cache_hit_ratio(),
        "models": models,
        "daily": daily,
    });
    Ok((StatusCode(200), body.to_string()))
}

#[cfg(test)]
#[path = "../../tests/inline/web_tokens.rs"]
mod tests;
