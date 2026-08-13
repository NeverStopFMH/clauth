#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::providers::{StatRow, StatRowKind, ThirdPartyStats, UsageBar};
use crate::usage::UsageWindow;

fn window(util: f64, resets_at: Option<&str>) -> UsageWindow {
    UsageWindow {
        utilization: util,
        resets_at: resets_at.map(str::to_string),
    }
}

fn snapshot(name: &str, active: bool) -> ProfileSnapshot {
    ProfileSnapshot {
        name: name.to_string(),
        active,
        provider: "anthropic".to_string(),
        base_url: None,
        sub_type: Some("max".to_string()),
        headroom_pct: None,
    }
}

fn third_party_snapshot(name: &str, base_url: &str, headroom_pct: Option<f64>) -> ProfileSnapshot {
    ProfileSnapshot {
        name: name.to_string(),
        active: false,
        provider: "DeepSeek".to_string(),
        base_url: Some(base_url.to_string()),
        sub_type: None,
        headroom_pct,
    }
}

fn third_party_stats(
    bars: Vec<UsageBar>,
    rows: Vec<StatRow>,
    plan: Option<&str>,
) -> ThirdPartyStats {
    ThirdPartyStats {
        is_available: true,
        rows,
        bars,
        plan: plan.map(str::to_string),
        endpoint: None,
        best_effort: false,
    }
}

fn bar(label: &str, pct: f64) -> UsageBar {
    UsageBar {
        label: label.to_string(),
        pct,
        resets_at: None,
        used: None,
        total: None,
    }
}

fn row(label: &str, value: &str) -> StatRow {
    StatRow {
        label: label.to_string(),
        value: value.to_string(),
        kind: StatRowKind::Body,
    }
}

#[test]
fn third_party_headline_joins_bars_with_plan_prefix() {
    let s = third_party_stats(
        vec![bar("prompts", 50.0), bar("tokens", 12.4)],
        vec![],
        Some("pro"),
    );
    assert_eq!(third_party_headline(&s), "pro: prompts 50%, tokens 12.4%");
}

#[test]
fn third_party_headline_falls_back_to_first_row() {
    let s = third_party_stats(vec![], vec![row("balance", "$4.20")], None);
    assert_eq!(third_party_headline(&s), "balance: $4.20");
}

#[test]
fn third_party_headline_skips_value_less_heading_row() {
    // DeepSeek's first row is a value-less `USD balance` heading; the headline must
    // skip it and surface the first row that actually carries a value, never a
    // dangling `USD balance:` with nothing after it.
    let s = third_party_stats(
        vec![],
        vec![row("USD balance", ""), row("total", "$4.20")],
        None,
    );
    assert_eq!(third_party_headline(&s), "total: $4.20");
}

#[test]
fn third_party_headline_bare_plan_when_no_bars_or_rows() {
    // plan present, nothing else, still available → just the plan label.
    let s = third_party_stats(vec![], vec![], Some("pro"));
    assert_eq!(third_party_headline(&s), "pro");
}

#[test]
fn third_party_headline_unavailable_when_empty() {
    let mut s = third_party_stats(vec![], vec![], None);
    s.is_available = false;
    assert_eq!(third_party_headline(&s), "unavailable");
}

#[test]
fn live_footer_joins_present_parts() {
    let five = window(33.0, None);
    let seven = window(8.0, None);
    assert_eq!(
        live_footer(Some("work"), Some(&five), Some(&seven)),
        "active=work | 5h 33% used | 7d 8% used"
    );
}

#[test]
fn live_footer_omits_absent_parts() {
    assert_eq!(live_footer(None, None, None), "");
    assert_eq!(live_footer(Some("x"), None, None), "active=x");
}

#[test]
fn instructions_block_emits_stable_roster_cost_model_and_safety_prose() {
    let profiles = vec![snapshot("work", true), snapshot("personal", false)];
    let out = instructions_block(&profiles, &SessionAuth::Global);

    // roster: identity only, with the active marker, and one line per bracket.
    assert!(out.contains("- work (active), personal [anthropic, max]"));

    // the roster is labelled a session-start snapshot with a live-refresh pointer.
    assert!(out.contains("Profiles, most headroom first (session-start snapshot"));
    assert!(out.contains("call `list_profiles`"));

    // the tool router survives, because it is the ONLY clauth text a session is
    // guaranteed to hold: some harnesses defer tool schemas, so a description is
    // unloaded until something searches for it.
    assert!(
        out.contains("Tools: `list_profiles`") && out.contains("`delegate_result`"),
        "the tool router must name every tool: {out}",
    );
    // ...but per-tool mechanics belong in that tool's own description, which is
    // loaded by the time anyone can call it. Restating them here is the
    // duplication the router replaced.
    assert!(
        !out.contains("depth 1") && !out.contains("`job_id`"),
        "per-tool mechanics must not creep back into the router line: {out}",
    );

    // cost model is spelled out so delegate routing can account for money. All
    // three paid shapes are named: collapsing "api key" to one billing story
    // told the model an Alibaba plan profile costs per token, when its quota is
    // bought up front and a delegate there spends nothing extra.
    assert!(out.contains("Cost:"));
    assert!(
        out.contains("bills real money"),
        "billing must not name one currency: this operator holds DeepSeek \
         balances in both USD and CNY",
    );
    assert!(
        out.contains("prepaid plan quota"),
        "a prepaid plan must not read as pay-as-you-go: {out}",
    );

    // cheapest-target pointer must survive a prose edit.
    assert!(
        out.contains("`list_profiles` for live windows"),
        "the cheapest-target routing pointer must survive a prose edit",
    );

    // volatile figures are NOT baked in — they rot within a turn, so they must
    // stay on the per-call `list_profiles` path, never here.
    assert!(
        !out.contains("% used"),
        "no usage percentages in the boot block"
    );

    // the session-aware switch note must survive a prose edit (Global variant here).
    assert!(
        out.contains("switch & this session:"),
        "the `switch` effect note must survive a prose edit",
    );
    assert!(
        out.contains("its next token refresh"),
        "the global-session switch caveat must survive a prose edit",
    );
}

#[test]
fn roster_groups_identical_brackets_and_leads_with_most_headroom() {
    let url = "https://api.deepseek.com/anthropic";
    let profiles = vec![
        third_party_snapshot("spent", url, Some(2.0)),
        snapshot("oauth", false),
        third_party_snapshot("unknown", url, None),
        third_party_snapshot("fresh", url, Some(90.0)),
    ];
    let out = roster_lines(&profiles);

    // One line per bracket, and the shared endpoint prints as a host: 14 same
    // provider profiles otherwise repeat one identical URL 14 times.
    assert_eq!(
        out, "- fresh, spent, unknown [DeepSeek, api.deepseek.com]\n- oauth [anthropic, max]\n",
        "grouped, host-only, most headroom first",
    );

    // A profile clauth has no figure for must not outrank one it knows is nearly
    // spent: `None` is "unranked", never "full".
    let free = out.find("fresh").unwrap();
    let spent = out.find("spent").unwrap();
    let unknown = out.find("unknown").unwrap();
    assert!(free < spent && spent < unknown);

    // The `anthropic` group has no host at all, and its unknown headroom puts it
    // below a DeepSeek group whose best member is 90% free.
    assert!(!out.contains("https://"), "base urls print as hosts: {out}");
}

#[test]
fn session_auth_variants_shape_switch_note_and_runtime_paths() {
    // Global: warns the current session's identity changes on next refresh.
    let global = switch_effect(&SessionAuth::Global);
    assert!(global.contains("THIS session reads"));
    assert!(global.contains("next token refresh"));
    assert!(global.contains("use the `delegate` tool"));

    // Isolated runtime: names the pinned profile and states it is unaffected.
    let pinned = switch_effect(&SessionAuth::IsolatedRuntime("work".to_string()));
    assert!(pinned.contains("pinned to `work`"));
    assert!(pinned.contains("unaffected"));

    // Custom config dir: also unaffected, no profile name.
    let custom = switch_effect(&SessionAuth::IsolatedCustom);
    assert!(custom.contains("custom `CLAUDE_CONFIG_DIR`"));
    assert!(custom.contains("unaffected"));

    // The runtime-path note is earned by the one tier whose tree clauth builds.
    // A `Global` session has no runtime dir at all, and a custom
    // `CLAUDE_CONFIG_DIR` is somebody else's layout — claiming the symlink
    // forest for either would send a model editing a path that does not exist,
    // or describe a foreign tree it has never read.
    let profiles = vec![snapshot("work", true)];
    let runtime_block = instructions_block(&profiles, &SessionAuth::IsolatedRuntime("work".into()));
    assert!(
        runtime_block.contains("runtime paths:"),
        "the runtime-path note must reach the rendered block: {runtime_block}",
    );
    assert!(
        runtime_block.contains("(`$CLAUDE_CONFIG_DIR`, profile `work`)"),
        "the note must name this session's profile and point at the env var \
         holding its real dir: the on-disk name carries a per-session suffix, so \
         any literal path spelled in the note would not exist",
    );
    assert!(
        !runtime_block.contains("/runtime/"),
        "no constructed runtime path: the real dir is `runtime-<sid>-<n>`",
    );
    for other in [SessionAuth::Global, SessionAuth::IsolatedCustom] {
        assert!(runtime_paths_note(&other).is_none());
        assert!(
            !instructions_block(&profiles, &other).contains("runtime paths:"),
            "only an isolated `clauth start` runtime may claim the symlink layout",
        );
    }
}
