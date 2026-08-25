//! Inline tests for `plugin_host`. No environment needed: these pin the
//! compile-time wiring (derive metadata, the embedded tree) and the committed
//! SessionStart hook that points at `clauth self-heal`. The lifecycle itself
//! (the real `claude` CLI as transaction boundary) is pinned hermetically by
//! the fake-claude install test in `tui_app.rs` and exercised for real in the
//! scratch-profile verifies.

use super::ClauthPlugin;
use agentgear::PluginHost;

/// The derive and the one-line `build.rs` are the whole of the agentgear
/// wiring; if either silently broke (name drift, a version guard that stopped
/// pinning, an embed that stopped baking the tree), these fail without
/// spawning the binary.
#[test]
fn derive_metadata_is_wired() {
    assert_eq!(ClauthPlugin::NAME, "clauth");
    assert_eq!(ClauthPlugin::MARKETPLACE, "clauth");
    assert_eq!(ClauthPlugin::AGENTS, &["claude"]);
    // build.rs pins plugins/.claude-plugin/plugin.json `version` to this, so
    // the const equals the crate version.
    assert_eq!(ClauthPlugin::VERSION, env!("CARGO_PKG_VERSION"));

    let descriptor = ClauthPlugin::descriptor();
    assert_eq!(descriptor.name, "clauth");
    assert_eq!(descriptor.id(), "clauth@clauth");
    assert_eq!(descriptor.version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn embedded_tree_is_baked_in() {
    // The `embed` feature compresses `plugins/` into the binary; an empty blob
    // would mean `install(Scope::User, Source::Embedded)` errors at
    // materialize instead of installing.
    assert!(
        !ClauthPlugin::embedded_blob().is_empty(),
        "the plugin tree was not embedded"
    );
}

/// The SessionStart wiring the self-heal rides on: the committed hooks.json
/// must carry BOTH hooks — the profile-change note keeps working, and the new
/// self-heal entry points at the hidden `clauth self-heal` subcommand. A drift
/// here (someone edits hooks.json and drops one command) silently disables a
/// session behavior, which is exactly what this test exists to catch.
#[test]
fn session_start_hook_wires_self_heal_beside_the_note() {
    let hooks: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json"))
            .expect("plugins/hooks/hooks.json parses");
    let commands: Vec<String> = hooks["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart is an array")
        .iter()
        .filter_map(|group| group["hooks"].as_array())
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .map(str::to_string)
        .collect();
    assert!(
        commands
            .iter()
            .any(|c| c == "clauth hook-profile-changed-note"),
        "the profile-change note must keep its SessionStart slot: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "clauth self-heal"),
        "the self-heal hook is not wired into SessionStart: {commands:?}"
    );
}

/// The hook's output contract: a line appears only when the heal changed
/// something. Healthy means silent — the planted `if true` mutation that made
/// every heal print reds here — and a real transition (a stale marker cleared
/// after the registration vanished) prints exactly once, in the hook's own
/// wording. Runs hermetically through the fake-`claude` harness in
/// `testutil`.
#[cfg(unix)]
#[test]
fn self_heal_says_nothing_when_healthy_and_reports_changes() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);

    // A fresh install through the same host the TUI fix calls: registry entry
    // present, marker stamped.
    assert!(
        matches!(
            super::install().expect("install"),
            agentgear::Outcome::Installed
        ),
        "the fixture install must land"
    );

    // Healthy + marker present: nothing to say.
    assert_eq!(
        super::self_heal_line().expect("heal"),
        None,
        "a healthy install prints nothing"
    );

    // The registration's backing vanishes (registry entry + shim state), the
    // marker stays: the heal clears the stale marker and says so once.
    std::fs::remove_file(config.join("plugins").join("installed_plugins.json"))
        .expect("remove registry");
    std::fs::remove_file(std::env::var_os("CLAUDE_SHIM_STATE").expect("shim state pin"))
        .expect("remove shim state");
    assert_eq!(
        super::self_heal_line().expect("heal"),
        Some("clauth self-heal: cleared stale marker".to_string()),
        "a heal that changed something says so, in the hook's own wording"
    );

    // Marker cleared + nothing registered: silent again.
    assert_eq!(
        super::self_heal_line().expect("heal"),
        None,
        "after the clear there is nothing left to say"
    );
    let _ = &fake;
}
