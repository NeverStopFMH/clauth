//! The agentgear [`PluginHost`] derive plus the two lifecycle wrappers clauth
//! calls: the Plugin tab's one-key install and the SessionStart self-heal hook.
//!
//! clauth's plugin tree lives in `plugins/` (not the default `plugin/`), so the
//! derive's `tree` attr and `build.rs`'s `assert_plugin_version_at` both name
//! it. The tree itself stays a stock Claude Code plugin — `plugin.json` + the
//! `hooks/` dir — and agentgear supplies the lifecycle around it: materialize
//! the tree, drive `claude plugin marketplace add` + `plugin install`, verify
//! through `plugin list --json`, and stamp a marker self-heal keys on.
//!
//! The two `claude`-shelling paths here are the ONLY lifecycle call sites;
//! nothing else in the crate shells out to `claude plugin` (the Plugin tab's
//! probe reads the registry files directly, and the manual `mcpServers` fallback
//! is a settings write). The lifecycle is pinned hermetically by the
//! fake-`claude` tests in `tests/inline/tui_app.rs` and the self-heal pin in
//! `tests/inline/plugin_host.rs` — both `#[cfg(unix)]` (the fake CLI is a
//! shell shim), so a Windows CI leg does not run them.

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

#[cfg(test)]
#[path = "../tests/inline/plugin_host.rs"]
mod tests;
