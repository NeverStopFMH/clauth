#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::providers::{StatRow, StatRowKind, ThirdPartyStats, UsageBar};

fn snapshot(name: &str, active: bool) -> ProfileSnapshot {
    ProfileSnapshot {
        name: name.to_string(),
        active,
        provider: "anthropic".to_string(),
        base_url: None,
        sub_type: Some("max".to_string()),
        rank: RosterRank::Unknown,
    }
}

fn third_party_snapshot(name: &str, base_url: &str, rank: RosterRank) -> ProfileSnapshot {
    ProfileSnapshot {
        name: name.to_string(),
        active: false,
        provider: "DeepSeek".to_string(),
        base_url: Some(base_url.to_string()),
        sub_type: None,
        rank,
    }
}

/// A wallet-ranked snapshot, the shape the DeepSeek fleet takes.
fn wallet_snapshot(name: &str, currency: &str, amount: f64) -> ProfileSnapshot {
    third_party_snapshot(
        name,
        "https://api.deepseek.com/anthropic",
        RosterRank::Balance {
            currency: currency.to_string(),
            amount,
        },
    )
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
        third_party_snapshot("spent", url, RosterRank::Window(2.0)),
        snapshot("oauth", false),
        third_party_snapshot("unknown", url, RosterRank::Unknown),
        third_party_snapshot("fresh", url, RosterRank::Window(90.0)),
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

/// Wallet profiles rank by amount inside one currency and never across two. The
/// fleet this serves holds both: ordering 1117 CNY against 31 USD would need an
/// exchange rate clauth has no way to obtain, so currency groups fall back to
/// the order config first names them in — here CNY, because `cny-rich` leads.
#[test]
fn roster_ranks_wallets_within_a_currency_and_never_across_two() {
    // The amounts deliberately interleave across the two currencies: the biggest
    // number here is USD and the smallest is CNY. A fixture whose CNY amounts
    // all beat its USD ones cannot tell grouping apart from a plain sort on
    // magnitude, and would stay green with the currency boundary removed.
    let profiles = vec![
        wallet_snapshot("cny-big", "CNY", 300.0),
        wallet_snapshot("usd-small", "USD", 41.02),
        third_party_snapshot(
            "no-balance",
            "https://api.deepseek.com/anthropic",
            RosterRank::Unknown,
        ),
        wallet_snapshot("usd-big", "USD", 900.0),
        wallet_snapshot("cny-small", "CNY", 5.0),
    ];
    let out = roster_lines(&profiles);

    assert_eq!(
        out, "- cny-big, cny-small, usd-big, usd-small, no-balance [DeepSeek, api.deepseek.com]\n",
        "currency groups in config-first-seen order, amount descending inside each",
    );

    // The load-bearing half: a 5.00 CNY wallet still sorts above a 900 USD one,
    // because the group boundary decides placement before any amount does. A
    // comparator falling through to raw magnitude would lead with `usd-big`.
    let cny_small = out.find("cny-small").unwrap();
    let usd_big = out.find("usd-big").unwrap();
    assert!(cny_small < usd_big, "currency group outranks magnitude");
}

/// Every windowed profile outranks every wallet one, whatever the wallet holds.
/// A percentage and a balance measure different things, so the roster orders by
/// which KIND of figure it has before it compares any two numbers.
#[test]
fn a_spent_window_still_outranks_the_richest_wallet() {
    let url = "https://api.deepseek.com/anthropic";
    let profiles = vec![
        wallet_snapshot("rich", "CNY", 9999.0),
        third_party_snapshot("nearly-spent", url, RosterRank::Window(1.0)),
    ];
    let out = roster_lines(&profiles);
    assert_eq!(out, "- nearly-spent, rich [DeepSeek, api.deepseek.com]\n");
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
        "no constructed runtime path: the real dir is `runtime-<pid>-<seq>`",
    );
    // The note may name `~/.claude/` and nothing beyond it. Where a `~/.claude/`
    // entry chains on to is the operator's own layout, so a second destination
    // spelled here is true on the box that wrote it and false everywhere else.
    assert!(
        !runtime_block.contains("~/.agents"),
        "the note must not name a path clauth never builds: {runtime_block}",
    );
    for other in [SessionAuth::Global, SessionAuth::IsolatedCustom] {
        assert!(runtime_paths_note(&other).is_none());
        assert!(
            !instructions_block(&profiles, &other).contains("runtime paths:"),
            "only an isolated `clauth start` runtime may claim the symlink layout",
        );
    }
}

#[test]
fn live_usage_prose_names_every_window_and_warns() {
    let full = live_usage_prose(
        &serde_json::json!({"profile": "work", "5h_used_pct": 12.3, "7d_used_pct": 45.6}),
        "target",
    );
    assert_eq!(full, "target `work`: 5h 12.3% used, 7d 45.6% used");

    // A null window reads `unknown` (never drops out as if it were zero); a null
    // profile name reads `none` (no active profile configured, not a lost figure).
    let nulls = live_usage_prose(
        &serde_json::json!({"profile": null, "5h_used_pct": null, "7d_used_pct": null}),
        "active profile",
    );
    assert_eq!(nulls, "active profile none: 5h unknown, 7d unknown");

    let warned = live_usage_prose(
        &serde_json::json!({
            "profile": "work",
            "5h_used_pct": 12.0,
            "7d_used_pct": 45.6,
            "throughput_warning": "⚠ throughput: deepseek-chat degraded"
        }),
        "target",
    );
    assert_eq!(
        warned,
        "target `work`: 5h 12% used, 7d 45.6% used; ⚠ throughput: deepseek-chat degraded"
    );
}

#[test]
fn list_profiles_prose_renders_each_row_with_unknown_for_null_fields() {
    let solo = serde_json::json!({
        "name": "solo",
        "active": true,
        "provider": "anthropic",
        "tier": null,
        "windows": [],
        "third_party": null
    });
    let vendor = serde_json::json!({
        "name": "vendor",
        "active": false,
        "provider": "DeepSeek",
        "tier": null,
        "host": "api.deepseek.com",
        "windows": [
            {"label": "5h", "utilization_pct": 12.3, "resets_at": null},
            {"label": "7d", "utilization_pct": 45.0, "resets_at": "2026-08-13T00:00:00Z"}
        ],
        "third_party": "pro: 5h 50% used",
        "has_live_session": true,
        "throughput": [{
            "model": "deepseek-chat",
            "tok_s": 12.3,
            "samples": 5,
            "degraded": true,
            "rate_limited_recent": false,
            "retry_after_s": null
        }]
    });
    let text = list_profiles_prose(&serde_json::json!({"profiles": [solo, vendor]}));
    assert_eq!(
        text,
        "- solo (active) [anthropic]: usage unknown; tier unknown\n\
         - vendor [DeepSeek, api.deepseek.com]: 5h 12.3% used, 7d 45% used (resets at 2026-08-13T00:00:00Z); pro: 5h 50% used; live session; throughput: `deepseek-chat` 12.3 tok/s (degraded)"
    );
}

#[test]
fn list_profiles_prose_handles_empty_roster_and_error_envelope() {
    assert_eq!(
        list_profiles_prose(&serde_json::json!({"profiles": []})),
        "no profiles"
    );
    assert_eq!(
        list_profiles_prose(
            &serde_json::json!({"ok": false, "reason": "profile not found: ghost"})
        ),
        "error: profile not found: ghost"
    );
}

#[test]
fn which_prose_names_identity_and_usage() {
    let p = serde_json::json!({
        "profile": "kerry",
        "source": "session_dir",
        "tier": "Free",
        "throughput": [],
        "live_usage": {"profile": "kerry", "5h_used_pct": 12.0, "7d_used_pct": null}
    });
    assert_eq!(
        which_prose(&p),
        "session profile `kerry`, source `session_dir`, tier `Free`; active profile `kerry`: 5h 12% used, 7d unknown"
    );
}

#[test]
fn which_prose_says_unknown_when_unresolved() {
    let p = serde_json::json!({
        "profile": null,
        "source": null,
        "tier": null,
        "throughput": [],
        "live_usage": {"profile": null, "5h_used_pct": null, "7d_used_pct": null}
    });
    assert_eq!(
        which_prose(&p),
        "session profile unknown, source unknown, tier unknown; active profile none: 5h unknown, 7d unknown"
    );
}

/// A healthy row is the model's name and rate. `degraded` / `rate_limited_recent`
/// / `samples` are payload fields a healthy row spells as `false` or noise, so
/// none may reach the prose: a spelled-out `false` costs tokens for nothing.
#[test]
fn throughput_prose_healthy_row_is_name_and_rate_only() {
    let rows = vec![serde_json::json!({
        "model": "default",
        "tok_s": 64.5,
        "samples": 4,
        "degraded": false,
        "rate_limited_recent": false,
        "retry_after_s": null
    })];
    let out = throughput_prose(&rows);
    assert_eq!(out, "`default` 64.5 tok/s");
    assert!(
        !out.contains("degraded")
            && !out.contains("rate_limited")
            && !out.contains("samples")
            && !out.contains("false"),
        "a healthy row must not spell its false flags or its sample count: {out}",
    );
}

/// Flags appear as words only when true, and the retry delay rides with the
/// rate-limit flag, never alone.
#[test]
fn throughput_prose_flags_appear_only_when_true_with_retry_delay() {
    let rows = vec![
        serde_json::json!({
            "model": "a",
            "tok_s": 12.3,
            "samples": 5,
            "degraded": true,
            "rate_limited_recent": false,
            "retry_after_s": null
        }),
        serde_json::json!({
            "model": "b",
            "tok_s": 0.0,
            "samples": 0,
            "degraded": false,
            "rate_limited_recent": true,
            "retry_after_s": 30
        }),
        serde_json::json!({
            "model": "c",
            "tok_s": 1.1,
            "samples": 2,
            "degraded": true,
            "rate_limited_recent": true,
            "retry_after_s": null
        }),
    ];
    assert_eq!(
        throughput_prose(&rows),
        "`a` 12.3 tok/s (degraded); `b` 0 tok/s (rate-limited recently, retry in 30s); `c` 1.1 tok/s (degraded, rate-limited recently)"
    );
}

/// The documented usage fields read as English; a field claude added that
/// clauth does not document keeps its name in backticks so no figure vanishes.
#[test]
fn usage_prose_documented_fields_read_english_and_unknown_keys_survive() {
    let u = serde_json::json!({
        "input_tokens": 100,
        "output_tokens": 50,
        "cache_read_input_tokens": 30
    });
    assert_eq!(
        usage_prose(&u),
        "input 100 tokens, output 50 tokens, `cache_read_input_tokens` 30"
    );
}

/// `which` keeps its throughput clause (one profile's rows, and the rate itself
/// is news), but a healthy row reads as name + rate with no false flags.
#[test]
fn which_prose_throughput_names_healthy_rows_without_false_flags() {
    let p = serde_json::json!({
        "profile": "kerry",
        "source": "refresh_match",
        "tier": "Max 20x",
        "throughput": [{
            "model": "default",
            "tok_s": 64.5,
            "samples": 4,
            "degraded": false,
            "rate_limited_recent": false,
            "retry_after_s": null
        }],
        "live_usage": {"profile": "kerry", "5h_used_pct": 19.0, "7d_used_pct": 95.0}
    });
    let out = which_prose(&p);
    assert!(
        out.contains("throughput: `default` 64.5 tok/s"),
        "which keeps the rate for a healthy model: {out}",
    );
    assert!(
        !out.contains("degraded") && !out.contains("rate_limited") && !out.contains("samples"),
        "no false flags or sample counts in which prose: {out}",
    );
}

#[test]
fn switch_prose_renders_success_and_failure() {
    let ok = serde_json::json!({
        "ok": true,
        "previous": null,
        "active": "work",
        "live_usage": {"profile": "work", "5h_used_pct": 12.0, "7d_used_pct": null}
    });
    assert_eq!(
        switch_prose(&ok),
        "switched the global active profile from none to `work`; active profile `work`: 5h 12% used, 7d unknown"
    );

    let err = serde_json::json!({
        "ok": false,
        "reason": "profile not found: ghost",
        "live_usage": {"profile": null, "5h_used_pct": null, "7d_used_pct": null}
    });
    assert_eq!(
        switch_prose(&err),
        "switch failed: profile not found: ghost; active profile none: 5h unknown, 7d unknown"
    );
}

#[test]
fn delegate_prose_renders_background_depth_and_sync_envelope() {
    let bg = serde_json::json!({
        "job_id": "d-42-0",
        "profile": "work",
        "status": "running",
        "started_at": 123
    });
    assert_eq!(
        delegate_prose(&bg),
        "delegate to `work` running, job `d-42-0`"
    );

    let depth = serde_json::json!({
        "profile": "any",
        "is_error": true,
        "result": "delegation depth exceeded (max 1)"
    });
    assert_eq!(
        delegate_prose(&depth),
        "delegate to `any` failed: delegation depth exceeded (max 1)"
    );

    let sync = serde_json::json!({
        "profile": "work",
        "is_error": false,
        "result": "all done",
        "total_cost_usd": 0.5,
        "usage": {"input_tokens": 100, "output_tokens": 50},
        "live_usage": {"profile": "work", "5h_used_pct": 12.0, "7d_used_pct": 45.6}
    });
    assert_eq!(
        delegate_prose(&sync),
        "delegate to `work` finished: all done (cost $0.5), usage: input 100 tokens, output 50 tokens; target `work`: 5h 12% used, 7d 45.6% used"
    );
}

#[test]
fn delegate_result_prose_renders_running_invalid_and_done() {
    let running = serde_json::json!({
        "job_id": "d-7",
        "status": "running",
        "elapsed_secs": 7,
        "quota": [{"label": "5h", "utilization_pct": 12.0, "resets_at": null}]
    });
    assert_eq!(
        delegate_result_prose(&running),
        "job `d-7` running, elapsed 7s; quota: 5h 12% used"
    );

    let invalid = serde_json::json!({"is_error": true, "result": "invalid job_id"});
    assert_eq!(delegate_result_prose(&invalid), "error: invalid job_id");

    let done = serde_json::json!({
        "profile": "work",
        "is_error": false,
        "result": "done",
        "total_cost_usd": 1.25,
        "live_usage": {"profile": "work", "5h_used_pct": 12.0, "7d_used_pct": 45.6}
    });
    assert_eq!(
        delegate_result_prose(&done),
        "delegate to `work` finished: done (cost $1.25); target `work`: 5h 12% used, 7d 45.6% used"
    );
}

/// A bare scalar self-report (wrapped under `result` by the fold) reaches the
/// prose caller as its literal; a non-string one must never drop to `unknown`.
#[test]
fn delegate_result_prose_renders_a_wrapped_scalar_self_report() {
    let wrapped = serde_json::json!({
        "result": "unauthorized",
        "live_usage": {"profile": "work", "5h_used_pct": null, "7d_used_pct": null}
    });
    assert_eq!(
        delegate_result_prose(&wrapped),
        "delegate to `work` finished: unauthorized; target `work`: 5h unknown, 7d unknown"
    );

    let numeric = serde_json::json!({
        "result": 42,
        "live_usage": {"profile": "work", "5h_used_pct": null, "7d_used_pct": null}
    });
    assert_eq!(
        delegate_result_prose(&numeric),
        "delegate to `work` finished: 42; target `work`: 5h unknown, 7d unknown"
    );
}
