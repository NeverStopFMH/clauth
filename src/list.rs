//! `clauth list` — a human-readable account table.
//!
//! Renders over the exact `serde_json::Value` body `daemon::build_status`
//! produces, the same one `clauth status --json` serializes, so the two
//! surfaces read one data path and cannot drift. Presentation only: it reads
//! the on-disk usage caches `build_status` reads and never fetches.

use anyhow::Result;

use crate::daemon::build_status;
use crate::format::format_pct;
use crate::profile::{AppConfig, load_config};

/// `clauth list [--all|--disabled]` — print the account table. `include_disabled`
/// mirrors `build_status`'s flag: disabled profiles are hidden by default (the
/// active profile is always kept, disabled or not).
pub(crate) fn run(include_disabled: bool) -> Result<()> {
    let config = load_config()?;
    let body = build_status(
        &config,
        config.state.refresh_interval_ms,
        None,
        include_disabled,
    );
    print!("{}", render_table(&config, &body));
    Ok(())
}

/// One rendered table row, sourced from a single `build_status` profile entry
/// (plus `config` for the disabled flag, which the JSON body does not carry).
struct Row {
    /// `*` for the active profile, a space otherwise.
    marker: char,
    name: String,
    /// Tier for an anthropic account (`Max 5x`), else the provider name for a
    /// third-party one. Reading `entry["tier"]` keeps this in lockstep with
    /// `status`. A canceled subscription reads as its post-cancellation tier
    /// (`Free`) here; [`Row::state_suffix`] is what names the cancellation.
    plan: String,
    /// 5h / 7d window utilization as `NN%` (share consumed), `-` when no cache.
    five_h: String,
    seven_d: String,
    /// The third-party base url, or `-` for the default Anthropic endpoint.
    endpoint: String,
    disabled: bool,
    canceled: bool,
}

impl Row {
    fn from_entry(config: &AppConfig, entry: &serde_json::Value) -> Row {
        let name = entry["name"].as_str().unwrap_or("?");
        let provider = entry["provider"].as_str().unwrap_or("");
        let windows = entry["windows"].as_array();
        Row {
            marker: if entry["active"].as_bool() == Some(true) {
                '*'
            } else {
                ' '
            },
            name: name.to_string(),
            plan: entry["tier"].as_str().unwrap_or(provider).to_string(),
            five_h: window_pct(windows, crate::usage::LABEL_5H),
            seven_d: window_pct(windows, crate::usage::LABEL_7D),
            endpoint: entry["base_url"].as_str().unwrap_or("-").to_string(),
            disabled: config.find(name).is_some_and(|p| p.is_disabled()),
            canceled: crate::profile_json::is_canceled_cached(name),
        }
    }

    /// Trailing state marker: `(disabled)`, `(canceled)`, or `(disabled,
    /// canceled)`. Both render rather than one winning — an operator usually
    /// disables an account BECAUSE it died, so letting `disabled` mask
    /// `canceled` is the erasure the Fallback tab's stacked pills already exist
    /// to prevent. This table has no status column, so the suffix is the only
    /// place either fact can appear.
    fn state_suffix(&self) -> String {
        let states: Vec<&str> = [(self.disabled, "disabled"), (self.canceled, "canceled")]
            .into_iter()
            .filter_map(|(on, label)| on.then_some(label))
            .collect();
        if states.is_empty() {
            return String::new();
        }
        format!(" ({})", states.join(", "))
    }
}

/// The `utilization_pct` of the window labeled `label`, formatted via
/// [`format_pct`] (drops trailing `.0`); `-` when the profile has no cache
/// or no such window.
fn window_pct(windows: Option<&Vec<serde_json::Value>>, label: &str) -> String {
    windows
        .and_then(|ws| ws.iter().find(|w| w["label"].as_str() == Some(label)))
        .and_then(|w| w["utilization_pct"].as_f64())
        .map_or_else(|| "-".to_string(), format_pct)
}

/// Minimum column width: the header vs every cell, counted in `char`s so a
/// multibyte profile name still aligns.
fn col_width<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells
        .map(|c| c.chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(0)
}

fn render_table(config: &AppConfig, body: &serde_json::Value) -> String {
    let empty = Vec::new();
    let entries = body["profiles"].as_array().unwrap_or(&empty);
    if entries.is_empty() {
        return "no accounts yet. add one with `clauth login <name>`.\n".to_string();
    }

    let rows: Vec<Row> = entries.iter().map(|e| Row::from_entry(config, e)).collect();

    // Endpoint is the last column, so it is never padded and needs no width.
    let w_name = col_width("PROFILE", rows.iter().map(|r| r.name.as_str()));
    let w_plan = col_width("PLAN", rows.iter().map(|r| r.plan.as_str()));
    let w_5h = col_width("5H", rows.iter().map(|r| r.five_h.as_str()));
    let w_7d = col_width("7D", rows.iter().map(|r| r.seven_d.as_str()));

    // Two leading columns: the 1-char active marker and a separating space.
    let mut out = format!(
        "  {:<w_name$}  {:<w_plan$}  {:>w_5h$}  {:>w_7d$}  ENDPOINT\n",
        "PROFILE", "PLAN", "5H", "7D",
    );
    for r in &rows {
        out.push_str(&format!(
            "{} {:<w_name$}  {:<w_plan$}  {:>w_5h$}  {:>w_7d$}  {}{}\n",
            r.marker,
            r.name,
            r.plan,
            r.five_h,
            r.seven_d,
            r.endpoint,
            r.state_suffix(),
        ));
    }
    out
}

#[cfg(test)]
#[path = "../tests/inline/list.rs"]
mod tests;
