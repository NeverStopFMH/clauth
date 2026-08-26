//! The agentgear [`PluginHost`] derive plus the three lifecycle wrappers clauth
//! calls: the Plugin tab's one-key install, the SessionStart self-heal hook, and
//! the `clauth start` pre-flight that heals a broken registration before
//! `claude` launches (the hook cannot — a marketplace that fails to load means
//! the plugin never loads, so the hook never fires).
//!
//! clauth's plugin tree lives in `plugins/` (not the default `plugin/`), so the
//! derive's `tree` attr and `build.rs`'s `assert_plugin_version_at` both name
//! it. The tree itself stays a stock Claude Code plugin — `plugin.json` + the
//! `hooks/` dir — and agentgear supplies the lifecycle around it: materialize
//! the tree, drive `claude plugin marketplace add` + `plugin install`, verify
//! through `plugin list --json`, and stamp a marker self-heal keys on.
//!
//! The `claude`-shelling paths here are the ONLY lifecycle call sites;
//! nothing else in the crate shells out to `claude plugin` (the Plugin tab's
//! probe reads the registry files directly, and the manual `mcpServers` fallback
//! is a settings write). The lifecycle is pinned hermetically by the
//! fake-`claude` tests in `tests/inline/tui_app.rs` and the self-heal pin in
//! `tests/inline/plugin_host.rs` — both `#[cfg(unix)]` (the fake CLI is a
//! shell shim), so a Windows CI leg does not run them.

use std::path::{Path, PathBuf};

use agentgear::{Outcome, PluginHost, Scope, Source};

/// The plugin host for the committed `plugins/` tree. Claude-only: the default
/// `agents` list already names just `claude`, so no agent feature flags beyond
/// the crate defaults (derive + claude + embed) are enabled.
#[derive(PluginHost)]
#[plugin(name = "clauth", tree = "$CARGO_MANIFEST_DIR/plugins")]
pub(crate) struct ClauthPlugin;

/// The Plugin tab's one-key install: a user-scope install from the embedded
/// tree. The single spelling site — the tab's confirm handler and its pin test
/// both go through here, so `Scope::User` + `Source::Embedded` live in one
/// place and the copy-paste hint they replace has no other home to drift into.
pub(crate) fn install() -> anyhow::Result<Outcome> {
    Ok(ClauthPlugin::install(Scope::User, Source::Embedded)?)
}

/// The SessionStart hook body (`clauth self-heal`). Repairs a broken
/// registration, never resurrects an uninstall — agentgear's marker gate makes
/// a deliberately removed plugin stay removed. A healthy session prints
/// nothing, so a hook that fires on every session start injects no noise into
/// the conversation; a repair (or a failure) is worth saying out loud.
pub(crate) fn self_heal() -> anyhow::Result<()> {
    if let Some(line) = self_heal_line()? {
        crate::out::outln!("{line}");
    }
    Ok(())
}

/// What the hook says, or `None` when there is nothing to say: the outcome
/// becomes a line only when the heal changed something. Split from
/// [`self_heal`] so a test can pin the contract without a terminal.
pub(crate) fn self_heal_line() -> anyhow::Result<Option<String>> {
    let outcome = ClauthPlugin::self_heal()?;
    Ok((!matches!(outcome, Outcome::NoOp)).then(|| format!("clauth self-heal: {outcome}")))
}

/// The `clauth start` pre-flight: the migration trigger that heals a broken or
/// divergent clauth marketplace registration before `claude` launches. The hook
/// self-heal cannot be this trigger — a marketplace that fails to load means the
/// plugin never loads, so the hook never fires.
///
/// The gate below is two plain registry reads; a healthy registration spawns
/// nothing. A heal failure is logged and never fails the start: the session
/// still launches, and the hook (once the plugin loads again) keeps trying.
pub(crate) fn preflight() {
    if !preflight_gate() {
        return;
    }
    if let Err(e) = self_heal() {
        crate::logline::logline!("clauth: plugin pre-flight heal failed: {e:#}");
    }
}

/// Whether the pre-flight should run the heal: `true` for every registry shape a
/// user-scope heal converges, `false` only for a registration already sitting at
/// agentgear's materialized `current@claude` pointer with its generated manifest
/// present and every user-scope plugin entry's files resolvable. Read-only; a
/// file this cannot read counts as "heal" (conservative — the heal is idempotent,
/// so a needless run costs nothing but its own reads).
pub(crate) fn preflight_gate() -> bool {
    let Some(dir) = registry_dir() else {
        return true;
    };
    let Some(expected) = expected_pointer() else {
        return true;
    };
    marketplace_needs_heal(&dir, &expected) || plugin_entries_need_heal(&dir)
}

/// The materialized pointer agentgear's `materialize` publishes: its locked
/// layout is `<data_dir>/<plugin>/current@<client>` (agentgear design
/// §materialize), which the lifecycle's own re-point logic compares against
/// too. Derived here rather than called because clauth builds against the
/// published agentgear crate, and the layout is the contract either way.
pub(crate) fn expected_pointer() -> Option<PathBuf> {
    dirs::data_dir().map(|base| base.join("clauth").join("current@claude"))
}

/// The config dir the heal itself operates on. agentgear's CLI wrapper keeps
/// `CLAUDE_CONFIG_DIR` in the child env, so the gate must read the same dir CC
/// resolves: the non-empty override when one is set, else `~/.claude`. An empty
/// override is "cannot tell" — the heal's own guard refuses it with a named
/// error, which is the report a start should surface rather than a silent skip.
fn registry_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        Some(_) => None,
        None => crate::profile::claude_dir().ok(),
    }
}

/// The marketplace half of the gate: the `clauth` entry must be a directory
/// source registered exactly at the materialized pointer, with its generated
/// manifest present. Absent, github-sourced, diverged, or manifest-deleted all
/// heal — that is the deadlock the migration exists to break (a github entry's
/// next catalog refresh pulls a tree without the manifest and the plugin loads 0
/// hooks).
fn marketplace_needs_heal(dir: &Path, expected: &Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join("plugins").join("known_marketplaces.json")) else {
        return true;
    };
    let Ok(doc): Result<serde_json::Value, _> = serde_json::from_slice(&bytes) else {
        return true;
    };
    let Some(entry) = doc.get("clauth") else {
        return true;
    };
    if entry["source"]["source"].as_str() != Some("directory") {
        return true;
    }
    let Some(path) = entry["source"]["path"].as_str() else {
        return true;
    };
    Path::new(path) != expected
        || !Path::new(path)
            .join(".claude-plugin")
            .join("marketplace.json")
            .exists()
}

/// The plugin half of the gate: a user-scope `clauth@clauth` entry whose files or
/// load state are gone. A per-session config dir leaves exactly this behind when
/// its runtime tree is collected — the entry survives, its `installPath` dies —
/// and only the heal can rewrite it. Project-scope entries never decide here: the
/// heal is user-scope and cannot fix them, so counting them would churn a heal
/// every start for nothing.
fn plugin_entries_need_heal(dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join("plugins").join("installed_plugins.json")) else {
        return true;
    };
    let Ok(doc): Result<serde_json::Value, _> = serde_json::from_slice(&bytes) else {
        return true;
    };
    let Some(rows) = doc["plugins"]["clauth@clauth"].as_array() else {
        return false;
    };
    rows.iter()
        .filter(|row| row["scope"].as_str().unwrap_or("user") == "user")
        .any(|row| {
            !row["errors"].as_array().is_none_or(Vec::is_empty)
                || row["installPath"]
                    .as_str()
                    .is_none_or(|p| !Path::new(p).exists())
        })
}

#[cfg(test)]
#[path = "../tests/inline/plugin_host.rs"]
mod tests;
