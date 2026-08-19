//! Inline tests for `crate::pricing` — genai-prices distill, match-clause
//! resolution, constraint selection, snapshot history, and per-model cost
//! math. No network: tables are built from literals and the trimmed real-data
//! fixture.

use super::*;

use crate::testutil::HomeSandbox;

/// Trimmed real genai-prices v2 feed (`tests/fixtures/genai-v2-trimmed.json`):
/// four first-party providers plus two resellers, keeping the real `match`
/// clauses and price shapes.
const FIXTURE: &str = include_str!("../fixtures/genai-v2-trimmed.json");

// ── helpers ──────────────────────────────────────────────────────────────────

/// One unconstrained price entry at the given input/output rates (cache 0).
fn entry(input: f64, output: f64) -> PriceEntry {
    PriceEntry {
        input,
        output,
        cache_read: 0.0,
        cache_write: 0.0,
        constraint: None,
    }
}

/// An exact-match model at the given input/output rates.
fn eq_model(id: &str, input: f64, output: f64) -> PricedModel {
    PricedModel {
        id: id.to_owned(),
        match_: MatchClause::Equals(id.to_lowercase()),
        prices: vec![entry(input, output)],
    }
}

/// A table whose single snapshot (captured 2026-01-01) holds `models`, so any
/// query date resolves to them.
fn table(models: Vec<PricedModel>) -> PriceTable {
    PriceTable {
        models: models.clone(),
        history: vec![RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models,
        }],
        fetched_at_ms: 0,
    }
}

/// The future deepseek-v4 shape: off-peak is half price; peak 01:00–04:00 and
/// 06:00–10:00Z — two whole-hour windows, i.e. two entries.
fn two_window_model() -> PricedModel {
    PricedModel {
        id: "deepseek-v4-pro".to_owned(),
        match_: MatchClause::StartsWith("deepseek-v4-pro".to_owned()),
        prices: vec![
            entry(0.2175e-6, 0.435e-6), // off-peak fallback
            PriceEntry {
                input: 0.435e-6,
                output: 0.87e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "01:00Z".to_owned(), // missing seconds tolerated
                    end: "04:00Z".to_owned(),
                }),
            },
            PriceEntry {
                input: 0.435e-6,
                output: 0.87e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "06:00:00Z".to_owned(),
                    end: "10:00:00Z".to_owned(),
                }),
            },
        ],
    }
}

fn model(id: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> ModelTokens {
    ModelTokens {
        model: id.to_owned(),
        input,
        output,
        cache_read,
        cache_create,
    }
}

// ── distill ──────────────────────────────────────────────────────────────────

#[test]
fn distill_converts_mtok_to_per_token() {
    // Real deepseek-v4-pro entry: 0.435 USD/Mtok in → 4.35e-7 per token.
    let json = r#"[
        {"id": "deepseek", "models": [
            {"id": "deepseek-v4-pro",
             "match": {"or": [{"starts_with": "deepseek-v4-pro"}]},
             "prices": {"input_mtok": 0.435, "output_mtok": 0.87, "cache_read_mtok": 0.003625}}
        ]}
    ]"#;
    let models = distill(json).expect("distill ok");
    assert_eq!(models.len(), 1);
    let rate = &models[0].prices[0];
    assert!((rate.input - 4.35e-7).abs() < 1e-12, "got {}", rate.input);
    assert!((rate.output - 8.7e-7).abs() < 1e-12, "got {}", rate.output);
    assert!((rate.cache_read - 3.625e-9).abs() < 1e-15);
    assert_eq!(rate.cache_write, 0.0); // missing field defaults to 0
}

#[test]
fn distill_tiered_price_takes_base() {
    // claude-opus-4-6's real first entry is a {base, tiers} ladder; without a
    // per-request context window clauth prices the base (below-tier) rate.
    let json = r#"[
        {"id": "anthropic", "models": [
            {"id": "claude-opus-4-6",
             "match": {"or": [{"starts_with": "claude-opus-4-6"}]},
             "prices": [{"prices": {
                 "input_mtok": {"base": 5, "tiers": [{"start": 200000, "price": 10}]},
                 "output_mtok": 25}}]}
        ]}
    ]"#;
    let models = distill(json).expect("distill ok");
    assert!((models[0].prices[0].input - 5e-6).abs() < 1e-12);
    assert!((models[0].prices[0].output - 25e-6).abs() < 1e-12);
}

#[test]
fn distill_keeps_first_party_drops_resellers() {
    let json = r#"[
        {"id": "deepseek", "models": [
            {"id": "deepseek-v3.2", "match": {"equals": "deepseek-v3.2"},
             "prices": {"input_mtok": 0.28, "output_mtok": 0.42}}
        ]},
        {"id": "openrouter", "models": [
            {"id": "anthropic/claude-sonnet-4.5", "match": {"equals": "anthropic/claude-sonnet-4.5"},
             "prices": {"input_mtok": 3, "output_mtok": 15}}
        ]},
        {"id": "aws", "models": [
            {"id": "bedrock/claude-opus-4-8", "match": {"equals": "bedrock/claude-opus-4-8"},
             "prices": {"input_mtok": 5, "output_mtok": 25}}
        ]},
        {"id": "huggingface_together", "models": [
            {"id": "Qwen/Qwen3-Coder-480B-A35B-Instruct", "match": {"equals": "Qwen/Qwen3-Coder-480B-A35B-Instruct"},
             "prices": {"input_mtok": 1, "output_mtok": 2}}
        ]}
    ]"#;
    let models = distill(json).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["deepseek-v3.2"]);
}

#[test]
fn distill_parses_flat_and_conditional_prices() {
    // o3's real chain: an unconstrained entry then a start_date-constrained one.
    let json = r#"[
        {"id": "openai", "models": [
            {"id": "gpt-5", "match": {"equals": "gpt-5"},
             "prices": {"input_mtok": 1.25, "output_mtok": 10}},
            {"id": "o3", "match": {"or": [{"equals": "o3"}, {"equals": "o3-2025-04-16"}]},
             "prices": [
                {"prices": {"input_mtok": 10, "output_mtok": 40}},
                {"constraint": {"start_date": "2025-06-10"},
                 "prices": {"input_mtok": 2, "output_mtok": 8}}
             ]}
        ]}
    ]"#;
    let models = distill(json).expect("distill ok");
    assert_eq!(models.len(), 2);
    let o3 = &models[1];
    assert_eq!(o3.prices.len(), 2);
    assert_eq!(o3.prices[0].constraint, None);
    assert_eq!(
        o3.prices[1].constraint,
        Some(Constraint::StartDate("2025-06-10".to_owned()))
    );
    assert!((o3.prices[1].input - 2e-6).abs() < 1e-12);
}

#[test]
fn distill_skips_malformed_entries_not_the_model() {
    // A malformed CONDITIONAL entry (unknown constraint key) skips only
    // itself; the good sibling entry keeps the model priced.
    let json = r#"[
        {"id": "deepseek", "models": [
            {"id": "probe", "match": {"equals": "probe"},
             "prices": [
                {"prices": {"input_mtok": 10, "output_mtok": 20}},
                {"constraint": {"end_date": "2026-01-01"},
                 "prices": {"input_mtok": 30, "output_mtok": 40}}
             ]}
        ]}
    ]"#;
    let models = distill(json).expect("good entry survives");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "probe");
    assert_eq!(models[0].prices.len(), 1);
    assert_eq!(models[0].prices[0].constraint, None);
    assert!((models[0].prices[0].input - 10e-6).abs() < 1e-12);
}

#[test]
fn distill_fails_when_no_models_survive() {
    // Only resellers → zero models → the fetch fails rather than shipping an
    // empty table.
    let json = r#"[
        {"id": "openrouter", "models": [
            {"id": "z-ai/glm-4.7", "match": {"equals": "z-ai/glm-4.7"},
             "prices": {"input_mtok": 0.5, "output_mtok": 1}}
        ]}
    ]"#;
    assert!(distill(json).is_err());
    assert!(distill("{}").is_err()); // object root (old format) is rejected
    assert!(distill("[]").is_err());
    assert!(distill("not json").is_err());
}

#[test]
fn distill_skips_unparseable_models_and_providers() {
    let json = r#"[
        {"id": "deepseek", "models": [
            {"id": "good", "match": {"equals": "good"},
             "prices": {"input_mtok": 0.28, "output_mtok": 0.42}},
            {"id": "bad-price-shape", "match": {"equals": "bad-price-shape"},
             "prices": {"input_mtok": "garbage"}},
            {"id": "no-match",
             "prices": {"input_mtok": 1}}
        ]},
        {"id": 42, "models": []},
        "not-a-provider",
        {"id": "zhipuai", "models": [
            {"id": "no-token-price", "match": {"equals": "no-token-price"},
             "prices": {"web_searches_kcount": 10}}
        ]}
    ]"#;
    let models = distill(json).expect("one good model survives");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["good"]);
}

// ── match clauses ────────────────────────────────────────────────────────────

#[test]
fn match_equals_is_case_insensitive() {
    let t = table(vec![eq_model("gpt-5.6-luna", 1e-6, 6e-6)]);
    assert_eq!(
        t.rate_at("gpt-5.6-luna", "2026-08-19", 0).map(|r| r.input),
        Some(1e-6)
    );
    assert_eq!(
        t.rate_at("GPT-5.6-LUNA", "2026-08-19", 0).map(|r| r.input),
        Some(1e-6)
    );
    assert!(t.rate_at("gpt-5.6", "2026-08-19", 0).is_none());
}

#[test]
fn match_starts_with() {
    let m = PricedModel {
        id: "deepseek-v4-pro".to_owned(),
        match_: MatchClause::StartsWith("deepseek-v4-pro".to_owned()),
        prices: vec![entry(4.35e-7, 8.7e-7)],
    };
    let t = table(vec![m]);
    assert!(t.rate_at("deepseek-v4-pro", "2026-08-19", 0).is_some());
    assert!(
        t.rate_at("deepseek-v4-pro-thinking", "2026-08-19", 0)
            .is_some()
    );
    assert!(t.rate_at("deepseek-v4", "2026-08-19", 0).is_none());
    assert!(t.rate_at("xdeepseek-v4-pro", "2026-08-19", 0).is_none());
}

#[test]
fn match_contains() {
    let m = PricedModel {
        id: "claude-opus-4-8".to_owned(),
        match_: MatchClause::Contains("opus".to_owned()),
        prices: vec![entry(5e-6, 25e-6)],
    };
    let t = table(vec![m]);
    assert!(t.rate_at("claude-opus-4-8", "2026-08-19", 0).is_some());
    assert!(t.rate_at("anthropic-opus-x", "2026-08-19", 0).is_some());
    assert!(t.rate_at("claude-sonnet-4-5", "2026-08-19", 0).is_none());
}

#[test]
fn match_ends_with() {
    let m = PricedModel {
        id: "glm-4.7-flash".to_owned(),
        match_: MatchClause::EndsWith("flash".to_owned()),
        prices: vec![entry(1e-6, 2e-6)],
    };
    let t = table(vec![m]);
    assert!(t.rate_at("glm-4.7-flash", "2026-08-19", 0).is_some());
    assert!(t.rate_at("flash", "2026-08-19", 0).is_some());
    assert!(t.rate_at("glm-4.7-flashx", "2026-08-19", 0).is_none());
}

#[test]
fn match_or() {
    let m = PricedModel {
        id: "either".to_owned(),
        match_: MatchClause::Or(vec![
            MatchClause::Equals("a".to_owned()),
            MatchClause::StartsWith("b".to_owned()),
        ]),
        prices: vec![entry(1e-6, 2e-6)],
    };
    let t = table(vec![m]);
    assert!(t.rate_at("a", "2026-08-19", 0).is_some());
    assert!(t.rate_at("b-1", "2026-08-19", 0).is_some());
    assert!(t.rate_at("c", "2026-08-19", 0).is_none());
}

#[test]
fn match_and() {
    let m = PricedModel {
        id: "glm-5.2".to_owned(),
        match_: MatchClause::And(vec![
            MatchClause::StartsWith("glm".to_owned()),
            MatchClause::Contains("5.2".to_owned()),
        ]),
        prices: vec![entry(1.4e-6, 4.4e-6)],
    };
    let t = table(vec![m]);
    assert!(t.rate_at("glm-5.2", "2026-08-19", 0).is_some());
    assert!(t.rate_at("glm-5.2-pro", "2026-08-19", 0).is_some());
    assert!(t.rate_at("glm-5", "2026-08-19", 0).is_none());
    assert!(t.rate_at("qwen-5.2", "2026-08-19", 0).is_none());
}

#[test]
fn match_first_wins_in_distilled_order() {
    // Both clauses hold for "overlap"; the FIRST model in distilled order wins.
    let first = PricedModel {
        id: "first".to_owned(),
        match_: MatchClause::StartsWith("over".to_owned()),
        prices: vec![entry(1e-6, 2e-6)],
    };
    let second = PricedModel {
        id: "second".to_owned(),
        match_: MatchClause::Equals("overlap".to_owned()),
        prices: vec![entry(3e-6, 4e-6)],
    };
    let t = table(vec![first, second]);
    assert_eq!(
        t.rate_at("overlap", "2026-08-19", 0).map(|r| r.input),
        Some(1e-6)
    );
}

#[test]
fn match_regex_dates_stamped_ids() {
    // gpt-5.6-luna's real or-clause: an exact id plus a date-stamp regex for
    // `gpt-5.6-luna-YYYY-MM-DD` variants.
    let luna = PricedModel {
        id: "gpt-5.6-luna".to_owned(),
        match_: MatchClause::Or(vec![
            MatchClause::Equals("gpt-5.6-luna".to_owned()),
            MatchClause::Regex(r"^gpt-5\.6-luna-\d{4}-\d{2}-\d{2}$".to_owned()),
        ]),
        prices: vec![entry(1e-6, 6e-6)],
    };
    let t = table(vec![luna]);
    assert!(t.rate_at("gpt-5.6-luna", "2026-08-19", 0).is_some());
    assert!(
        t.rate_at("gpt-5.6-luna-2026-05-14", "2026-08-19", 0)
            .is_some()
    );
    // Not a date stamp → the regex arm fails and nothing else matches.
    assert!(t.rate_at("gpt-5.6-luna-2026-05", "2026-08-19", 0).is_none());
    assert!(t.rate_at("gpt-5.5", "2026-08-19", 0).is_none());
}

#[test]
fn rate_strips_bracket_suffix_before_match() {
    // A real starts_with clause prices the bracketed context ids.
    let ds = PricedModel {
        id: "deepseek-v4-pro".to_owned(),
        match_: MatchClause::StartsWith("deepseek-v4-pro".to_owned()),
        prices: vec![entry(4.35e-7, 8.7e-7)],
    };
    // An exact clause must NOT match a non-context bracket, so the strip has
    // to be selective (digits + k/m only).
    let glm = eq_model("glm-5.2", 1.4e-6, 4.4e-6);
    let t = table(vec![ds, glm]);

    for id in [
        "deepseek-v4-pro[1m]",
        "deepseek-v4-pro[64k]",
        "deepseek-v4-pro[1M]",
        "deepseek-v4-pro[64K]",
        "glm-5.2[1m]",
    ] {
        assert!(t.rate_at(id, "2026-08-19", 0).is_some(), "{id}");
    }
    // Non-context brackets are left alone → the full id misses the clause.
    assert!(t.rate_at("glm-5.2[xm]", "2026-08-19", 0).is_none());
    assert!(t.rate_at("glm-5.2[1x]", "2026-08-19", 0).is_none());
    assert!(t.rate_at("glm-5.2[]", "2026-08-19", 0).is_none());
}

// ── constraint resolution ────────────────────────────────────────────────────

#[test]
fn start_date_chain_before_on_after() {
    // o3's real shape: unconstrained entry first, cheaper start_date entry last.
    let o3 = PricedModel {
        id: "o3".to_owned(),
        match_: MatchClause::Equals("o3".to_owned()),
        prices: vec![
            entry(10e-6, 40e-6),
            PriceEntry {
                input: 2e-6,
                output: 8e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::StartDate("2025-06-10".to_owned())),
            },
        ],
    };
    let t = table(vec![o3]);
    let input = |date: &str| t.rate_at("o3", date, 0).map(|r| r.input);
    assert_eq!(input("2025-06-09"), Some(10e-6)); // before: fallback entry
    assert_eq!(input("2025-06-10"), Some(2e-6)); // on the date: active
    assert_eq!(input("2026-08-19"), Some(2e-6)); // after: active
}

#[test]
fn time_window_hour_granularity_boundaries() {
    // deepseek-chat's real V3-era window: peak 00:30–16:30Z, off-peak else.
    // At hour granularity `(start_h,start_m) <= (h,0) < (end_h,end_m)`:
    // hour 0 is off-peak (00:00 < 00:30) and hour 16 is PEAK (16:00 < 16:30);
    // the :30 boundaries are half-mispriced by construction (documented).
    let chat = PricedModel {
        id: "deepseek-chat".to_owned(),
        match_: MatchClause::StartsWith("deepseek-chat".to_owned()),
        prices: vec![
            entry(0.135e-6, 0.55e-6),
            PriceEntry {
                input: 0.27e-6,
                output: 1.1e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "00:30:00Z".to_owned(),
                    end: "16:30:00Z".to_owned(),
                }),
            },
        ],
    };
    let t = table(vec![chat]);
    let input = |hour: u8| {
        t.rate_at("deepseek-chat", "2026-08-19", hour)
            .map(|r| r.input)
    };
    assert_eq!(input(0), Some(0.135e-6)); // off-peak
    assert_eq!(input(5), Some(0.27e-6)); // peak
    assert_eq!(input(16), Some(0.27e-6)); // peak: (16,0) < (16,30)
    assert_eq!(input(17), Some(0.135e-6)); // off-peak again
    assert_eq!(input(23), Some(0.135e-6)); // off-peak
}

#[test]
fn two_window_peak_offpeak_peak() {
    // Reversed-entry selection tries the 06:00 window first, then 01:00, then
    // the unconstrained off-peak fallback.
    let t = table(vec![two_window_model()]);
    let input = |hour: u8| {
        t.rate_at("deepseek-v4-pro", "2026-08-19", hour)
            .map(|r| r.input)
    };
    assert_eq!(input(1), Some(0.435e-6)); // peak, first window
    assert_eq!(input(4), Some(0.2175e-6)); // 04:00 is excluded (half-open)
    assert_eq!(input(5), Some(0.2175e-6)); // gap between windows
    assert_eq!(input(7), Some(0.435e-6)); // peak, second window
    assert_eq!(input(10), Some(0.2175e-6)); // 10:00 excluded
    assert_eq!(input(23), Some(0.2175e-6)); // off-peak
}

// ── snapshot history ─────────────────────────────────────────────────────────

#[test]
fn snapshot_picks_rate_live_on_date() {
    let history = vec![
        RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        },
        RateSnapshot {
            captured: "2026-06-01".to_owned(),
            models: vec![eq_model("m", 2e-6, 4e-6)],
        },
    ];
    let t = PriceTable {
        models: history[1].models.clone(),
        history,
        fetched_at_ms: 0,
    };
    let input = |date: &str| t.rate_at("m", date, 0).map(|r| r.input);
    assert_eq!(input("2026-05-31"), Some(1e-6)); // day before the change
    assert_eq!(input("2026-06-01"), Some(2e-6)); // the change date itself
    assert_eq!(input("2026-09-01"), Some(2e-6)); // after
}

#[test]
fn date_before_first_snapshot_uses_first() {
    let t = table(vec![eq_model("m", 1e-6, 2e-6)]);
    assert_eq!(t.rate_at("m", "2025-01-01", 0).map(|r| r.input), Some(1e-6));
}

#[test]
fn capture_appends_only_on_change() {
    let t = PriceTable::capture(
        vec![eq_model("m", 1e-6, 2e-6)],
        "2026-08-19".to_owned(),
        42,
        Vec::new(),
    );
    assert_eq!(t.history.len(), 1);

    // Identical refetch: same models → no new snapshot, capture date dropped.
    let t2 = PriceTable::capture(
        vec![eq_model("m", 1e-6, 2e-6)],
        "2026-08-20".to_owned(),
        43,
        t.history.clone(),
    );
    assert_eq!(t2.history.len(), 1);
    assert_eq!(t2.history[0].captured, "2026-08-19");

    // A rate change appends.
    let t3 = PriceTable::capture(
        vec![eq_model("m", 2e-6, 4e-6)],
        "2026-08-20".to_owned(),
        44,
        t2.history.clone(),
    );
    assert_eq!(t3.history.len(), 2);
    assert_eq!(t3.history[1].captured, "2026-08-20");
    // The working set is the newest snapshot's models.
    assert_eq!(
        t3.rate_at("m", "2026-08-20", 0).map(|r| r.input),
        Some(2e-6)
    );
}

#[test]
fn capture_caps_history_at_180() {
    let mut t = PriceTable::capture(
        vec![eq_model("m", 0.0, 0.0)],
        "2026-01-01".to_owned(),
        0,
        Vec::new(),
    );
    for i in 1..=182u32 {
        let rate = f64::from(i) * 1e-6;
        t = PriceTable::capture(
            vec![eq_model("m", rate, 0.0)],
            format!("2026-{i:03}"),
            u64::from(i),
            t.history,
        );
    }
    assert_eq!(t.history.len(), HISTORY_CAP);
    // 183 appends total: the three oldest (rates 0, 1, 2) dropped.
    assert!(
        (t.history[0].models[0].prices[0].input - 3e-6).abs() < 1e-12,
        "got {}",
        t.history[0].models[0].prices[0].input
    );
    assert!(
        (t.history[HISTORY_CAP - 1].models[0].prices[0].input - 182e-6).abs() < 1e-9,
        "got {}",
        t.history[HISTORY_CAP - 1].models[0].prices[0].input
    );
}

#[test]
fn cache_round_trip_preserves_history() {
    let sandbox = HomeSandbox::new();
    let history = vec![
        RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        },
        RateSnapshot {
            captured: "2026-06-01".to_owned(),
            models: vec![eq_model("m", 2e-6, 4e-6)],
        },
    ];
    let table = PriceTable {
        models: history[1].models.clone(),
        history,
        fetched_at_ms: 12345,
    };
    let path = sandbox
        .home()
        .join(".clauth")
        .join("genai_price_cache.json");
    save_cache(&path, &table);

    let loaded = load_cached().expect("cache loads");
    assert_eq!(loaded.fetched_at_ms, 12345);
    assert_eq!(loaded.history.len(), 2);
    assert_eq!(
        loaded.rate_at("m", "2026-08-19", 0).map(|r| r.input),
        Some(2e-6)
    );
    assert_eq!(
        loaded.rate_at("m", "2026-02-01", 0).map(|r| r.input),
        Some(1e-6)
    );
}

#[test]
fn load_cache_rejects_empty_history() {
    let sandbox = HomeSandbox::new();
    let path = sandbox
        .home()
        .join(".clauth")
        .join("genai_price_cache.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, r#"{"fetched_at_ms": 1, "history": []}"#).expect("write");
    assert!(load_cached().is_none());
}

#[test]
fn stale_litellm_cache_deleted_once() {
    let sandbox = HomeSandbox::new();
    let new_path = sandbox
        .home()
        .join(".clauth")
        .join("genai_price_cache.json");
    let stale = sandbox.home().join(".clauth").join("price_cache.json");
    std::fs::create_dir_all(stale.parent().expect("parent")).expect("mkdir");
    std::fs::write(&stale, "{}").expect("write");

    let mut done = false;
    delete_stale_cache_once(&new_path, &mut done);
    assert!(done);
    assert!(!stale.exists());

    // A second call is a no-op — the flag is set before the delete, so a
    // reappearing file is never re-deleted.
    std::fs::write(&stale, "{}").expect("write");
    delete_stale_cache_once(&new_path, &mut done);
    assert!(stale.exists());
}

#[test]
fn all_constrained_entries_fall_back_to_first() {
    // Upstream get_prices: when NO conditional entry is active, prices[0]
    // serves (never None, never a panic).
    let m = PricedModel {
        id: "future-only".to_owned(),
        match_: MatchClause::Equals("future-only".to_owned()),
        prices: vec![
            PriceEntry {
                input: 3e-6,
                output: 4e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::StartDate("2030-01-01".to_owned())),
            },
            PriceEntry {
                input: 9e-6,
                output: 10e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::StartDate("2031-01-01".to_owned())),
            },
        ],
    };
    let t = table(vec![m]);
    let rate = t
        .rate_at("future-only", "2026-08-19", 0)
        .expect("falls back to the first entry");
    assert_eq!(rate.input, 3e-6);
    assert_eq!(rate.output, 4e-6);
}

// ── real-data fixture ────────────────────────────────────────────────────────

#[test]
fn fixture_distills_resolvers_and_excludes_resellers() {
    let models = distill(FIXTURE).expect("fixture distills");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    // Resellers (openrouter, huggingface_together) are dropped entirely.
    assert!(!ids.contains(&"anthropic/claude-sonnet-4.5"));
    assert!(!ids.contains(&"Qwen/Qwen3-Coder-480B-A35B-Instruct"));
    assert!(ids.contains(&"deepseek-v4-pro"));
    assert!(ids.contains(&"GLM-5.2"));

    let t = PriceTable::capture(models, "2026-08-19".to_owned(), 0, Vec::new());
    // Bracket-stripped ids resolve.
    assert!(
        (t.rate_at("deepseek-v4-pro[1m]", "2026-08-19", 0)
            .expect("rate")
            .input
            - 4.35e-7)
            .abs()
            < 1e-12
    );
    assert!(t.rate_at("glm-5.2[1m]", "2026-08-19", 0).is_some());
    // o3's start_date chain.
    let o3 = |d: &str| t.rate_at("o3", d, 0).map(|r| r.input);
    assert_eq!(o3("2025-01-01"), Some(10e-6));
    assert_eq!(o3("2026-08-19"), Some(2e-6));
    // deepseek-chat's time window.
    let chat = |h: u8| t.rate_at("deepseek-chat", "2026-08-19", h).map(|r| r.input);
    assert_eq!(chat(0), Some(0.135e-6));
    assert_eq!(chat(5), Some(0.27e-6));
    // The regex clause keeps the date-stamped luna id priced.
    assert!(
        t.rate_at("gpt-5.6-luna-2026-08-01", "2026-08-19", 0)
            .is_some()
    );
    // claude-opus-4-6's tiered base resolves through the full path.
    assert_eq!(
        t.rate_at("claude-opus-4-6", "2026-08-19", 0)
            .map(|r| r.input),
        Some(5e-6)
    );
}

// ── cost ─────────────────────────────────────────────────────────────────────

#[test]
fn cost_sums_all_four_buckets() {
    // Clean rates: $1/$2/$0.10/$1.25 per million.
    let t = table(vec![PricedModel {
        id: "m".to_owned(),
        match_: MatchClause::Equals("m".to_owned()),
        prices: vec![PriceEntry {
            input: 1e-6,
            output: 2e-6,
            cache_read: 1e-7,
            cache_write: 1.25e-6,
            constraint: None,
        }],
    }]);
    let m = model("m", 1_000_000, 1_000_000, 1_000_000, 1_000_000);
    // 1.0 + 2.0 + 0.10 + 1.25 = 4.35
    let c = t.cost_at(&m, "2026-08-19", 0).expect("priced");
    assert!((c - 4.35).abs() < 1e-9, "got {c}");
}

#[test]
fn cost_none_for_unpriced_model() {
    let t = table(vec![PricedModel {
        id: "m".to_owned(),
        match_: MatchClause::Equals("m".to_owned()),
        prices: vec![PriceEntry {
            input: 1e-6,
            output: 2e-6,
            cache_read: 1e-7,
            cache_write: 1.25e-6,
            constraint: None,
        }],
    }]);
    assert!(
        t.cost_at(&model("unknown", 1000, 0, 0, 0), "2026-08-19", 0)
            .is_none()
    );
}

#[test]
fn cost_at_uses_dated_rate() {
    // A table with two snapshots: cost_at follows the date's rate.
    let history = vec![
        RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        },
        RateSnapshot {
            captured: "2026-06-01".to_owned(),
            models: vec![eq_model("m", 2e-6, 4e-6)],
        },
    ];
    let t = PriceTable {
        models: history[1].models.clone(),
        history,
        fetched_at_ms: 0,
    };
    let m = model("m", 1_000_000, 0, 0, 0);
    assert!((t.cost_at(&m, "2026-05-01", 0).expect("priced") - 1.0).abs() < 1e-9);
    assert!((t.cost_at(&m, "2026-06-01", 0).expect("priced") - 2.0).abs() < 1e-9);
}

#[test]
fn cost_day_prices_each_hour_at_its_rate() {
    // Peak hours 1-3 and 6-9 (7 hours) at $0.435/M, the other 17 at half.
    let t = table(vec![two_window_model()]);
    let mut hours = [HourTokens::default(); 24];
    for h in &mut hours {
        h.input = 1_000_000;
    }
    let cost = t
        .cost_day("deepseek-v4-pro", "2026-08-19", &hours)
        .expect("priced");
    let expected = 7.0 * 0.435 + 17.0 * 0.2175;
    assert!((cost - expected).abs() < 1e-9, "got {cost}");

    // An unpriced model is None even with tokens on the clock.
    assert!(t.cost_day("unknown", "2026-08-19", &hours).is_none());
}
