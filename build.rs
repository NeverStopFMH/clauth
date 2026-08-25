// agentgear's version guard: `plugins/.claude-plugin/plugin.json` must carry the
// crate version, or the build fails (CC keys its plugin cache on that version, so
// a mismatch would ship a silent no-op). Also tracks the tree for rebuilds and
// emits the marker the `#[derive(PluginHost)]` expansion checks for, so a build
// without this file is a compile error. The tree lives in `plugins/` rather than
// the default `plugin/`, so the derive's `tree` attr and this call name it twice.
fn main() {
    agentgear::build::assert_plugin_version_at(concat!(env!("CARGO_MANIFEST_DIR"), "/plugins"));
}
