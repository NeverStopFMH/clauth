//! Inline tests for `crate::pricing` — distill parsing, rate lookup with suffix
//! fallback, and per-model cost math. No network: every test builds a table from
//! a literal so the pure logic is exercised deterministically.

use super::*;

use std::collections::HashMap;

/// Build a `PriceTable` from `(id, input, output, cache_read, cache_write)` rows.
fn table(rows: &[(&str, f64, f64, f64, f64)]) -> PriceTable {
    let mut rates = HashMap::new();
    for &(id, input, output, cache_read, cache_write) in rows {
        rates.insert(
            id.to_owned(),
            ModelRate {
                input,
                output,
                cache_read,
                cache_write,
            },
        );
    }
    PriceTable {
        rates,
        providers: HashMap::new(),
        fetched_at_ms: 0,
    }
}

/// Same as [`table`] but also seeds the provider map for tests exercising
/// org-branded fallback (step f).
fn table_pv(rows: &[(&str, f64, f64, f64, f64)], providers: &[(&str, &str)]) -> PriceTable {
    let mut t = table(rows);
    for &(id, provider) in providers {
        t.providers.insert(id.to_owned(), provider.to_owned());
    }
    t
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

// ── distill ────────────────────────────────────────────────────────────────

#[test]
fn distill_keeps_priced_keys_including_namespaced() {
    let json = r#"{
        "claude-opus-4-8": {
            "input_cost_per_token": 0.000005,
            "output_cost_per_token": 0.000025,
            "cache_read_input_token_cost": 0.0000005,
            "cache_creation_input_token_cost": 0.00000625,
            "litellm_provider": "anthropic"
        },
        "deepseek-chat": {
            "input_cost_per_token": 0.00000028,
            "output_cost_per_token": 0.00000042,
            "cache_read_input_token_cost": 0.000000028,
            "cache_creation_input_token_cost": null,
            "litellm_provider": "deepseek"
        },
        "zai/glm-4.7": {
            "input_cost_per_token": 0.0000007,
            "output_cost_per_token": 0.0000028,
            "litellm_provider": "zai"
        },
        "openrouter/z-ai/glm-4.7": {
            "input_cost_per_token": 0.0000005,
            "litellm_provider": "openrouter"
        },
        "some-embedding-model": {
            "output_cost_per_token": 0.0
        }
    }"#;

    let (rates, providers) = distill(json).expect("distill ok");

    // Bare priced key kept with all four buckets.
    let opus = rates.get("claude-opus-4-8").expect("opus present");
    assert_eq!(opus.input, 0.000005);
    assert_eq!(opus.output, 0.000025);
    assert_eq!(opus.cache_read, 0.0000005);
    assert_eq!(opus.cache_write, 0.00000625);

    // Null cache-write defaults to 0.0 (e.g. DeepSeek auto-cache).
    let ds = rates.get("deepseek-chat").expect("deepseek present");
    assert_eq!(ds.cache_write, 0.0);

    // Namespaced keys are kept, not dropped.
    let glm = rates.get("zai/glm-4.7").expect("zai/glm-4.7 present");
    assert_eq!(glm.input, 0.0000007);
    assert!(rates.contains_key("openrouter/z-ai/glm-4.7"));

    // Provider map records litellm_provider for namespaced keys.
    assert_eq!(providers.get("zai/glm-4.7"), Some(&"zai".to_owned()));
    assert_eq!(
        providers.get("openrouter/z-ai/glm-4.7"),
        Some(&"openrouter".to_owned())
    );

    // Entries without input cost are still dropped.
    assert!(!rates.contains_key("some-embedding-model"));
}

#[test]
fn distill_rejects_non_object_root() {
    assert!(distill("[]").is_err());
    assert!(distill("not json").is_err());
}

// ── rate lookup ──────────────────────────────────────────────────────────────

#[test]
fn rate_exact_match() {
    let t = table(&[("claude-opus-4-8", 5e-6, 25e-6, 5e-7, 6.25e-6)]);
    assert_eq!(t.rate("claude-opus-4-8").map(|r| r.input), Some(5e-6));
}

#[test]
fn rate_strips_trailing_date_stamp() {
    let t = table(&[("claude-sonnet-4-5", 3e-6, 15e-6, 3e-7, 3.75e-6)]);
    // CC logs a date-stamped id; falls back to the bare family-version key.
    assert_eq!(
        t.rate("claude-sonnet-4-5-20250929").map(|r| r.output),
        Some(15e-6)
    );
}

#[test]
fn rate_strips_variant_suffix() {
    let t = table(&[("claude-opus-4-6", 5e-6, 25e-6, 5e-7, 6.25e-6)]);
    assert_eq!(
        t.rate("claude-opus-4-6-thinking").map(|r| r.input),
        Some(5e-6)
    );
}

#[test]
fn rate_unknown_model_is_none() {
    let t = table(&[("claude-opus-4-8", 5e-6, 25e-6, 5e-7, 6.25e-6)]);
    assert!(t.rate("gpt-5").is_none());
    assert!(t.rate("others").is_none());
}

#[test]
fn rate_fallback_never_matches_bare_family_key() {
    // A bare `claude` key must not wildcard-match every `claude-*` variant via
    // the suffix-strip fallback — only an exact lookup reaches it.
    let t = table(&[("claude", 1e-6, 2e-6, 0.0, 0.0)]);
    assert!(t.rate("claude-sonnet-4-5-20250929").is_none());
    assert_eq!(t.rate("claude").map(|r| r.input), Some(1e-6)); // exact still works
}

// ── official-provider resolution ───────────────────────────────────────────

#[test]
fn rate_bare_id_resolves_to_official_namespace() {
    // 'glm-4.7' has no bare key; the feed only has 'zai/glm-4.7'.
    // Step (e) official-namespace resolves glm → zai/glm-4.7.
    let t = table(&[("zai/glm-4.7", 7e-7, 2.8e-6, 0.0, 0.0)]);
    assert_eq!(t.rate("glm-4.7").map(|r| r.input), Some(7e-7));
}

#[test]
fn rate_bare_qwen_resolves_to_dashscope() {
    // 'qwen3.8-max' has no bare key and no 'qwen/' namespace; the feed lists it
    // under Alibaba's 'dashscope/'.
    let t = table(&[("dashscope/qwen3.8-max", 2e-6, 6e-6, 0.0, 0.0)]);
    assert_eq!(t.rate("qwen3.8-max").map(|r| r.input), Some(2e-6));
}

#[test]
fn rate_bare_kimi_resolves_to_moonshot() {
    let t = table(&[("moonshot/kimi-k2.5", 1e-6, 4e-6, 0.0, 0.0)]);
    assert_eq!(t.rate("kimi-k2.5").map(|r| r.input), Some(1e-6));
}

#[test]
fn rate_bare_grok_resolves_to_xai() {
    let t = table(&[("xai/grok-4", 3e-6, 15e-6, 0.0, 0.0)]);
    assert_eq!(t.rate("grok-4").map(|r| r.input), Some(3e-6));
}

#[test]
fn rate_dotted_qwen_rewrites_to_dashscope() {
    // 'qwen.qwen3-coder-next' is a bedrock_converse key; rewrite to dashscope.
    let t = table(&[("dashscope/qwen3-coder-next", 2e-6, 6e-6, 0.0, 0.0)]);
    assert_eq!(t.rate("qwen.qwen3-coder-next").map(|r| r.input), Some(2e-6));
}

#[test]
fn rate_org_branded_fallback_picks_lowest_official() {
    // glm-5.2 has no 'zai/glm-5.2' entry. The only zai-org-branded entry is
    // the Cloudflare-hosted one. Resellers are absent or filtered.
    let t = table_pv(
        &[
            ("cloudflare/@cf/zai-org/glm-5.2", 1.4e-6, 5.6e-6, 0.0, 0.0),
            ("fireworks_ai/glm-5p2", 1.0e-6, 2.0e-6, 0.0, 0.0),
        ],
        &[
            ("cloudflare/@cf/zai-org/glm-5.2", "cloudflare"),
            ("fireworks_ai/glm-5p2", "fireworks_ai"),
        ],
    );
    assert_eq!(t.rate("glm-5.2").map(|r| r.input), Some(1.4e-6));
}

#[test]
fn rate_org_branded_excludes_reseller_even_when_cheaper() {
    // The reseller key passes both the org-marker check (contains 'zai-org')
    // and the last-segment check (last segment == 'glm-5.2'), so only the
    // RESELLERS filter excludes it. The more expensive non-reseller entry wins.
    let t = table_pv(
        &[
            ("cloudflare/@cf/zai-org/glm-5.2", 1.4e-6, 5.6e-6, 0.0, 0.0),
            ("together_ai/zai-org/glm-5.2", 1.0e-6, 2.0e-6, 0.0, 0.0),
        ],
        &[
            ("cloudflare/@cf/zai-org/glm-5.2", "cloudflare"),
            ("together_ai/zai-org/glm-5.2", "together_ai"),
        ],
    );
    assert_eq!(t.rate("glm-5.2").map(|r| r.input), Some(1.4e-6));
}

#[test]
fn rate_org_branded_tie_breaks_lexicographically() {
    // Two non-reseller zai-org-branded entries at the SAME input price. HashMap
    // iteration order is random, so without the deterministic tie-break the pick
    // is a coin flip; the lexicographically smaller key must always win.
    let t = table_pv(
        &[
            ("cloudflare/@cf/zai-org/glm-5.2", 1.4e-6, 5.6e-6, 0.0, 0.0),
            ("novita/zai-org/glm-5.2", 1.4e-6, 2.0e-6, 0.0, 0.0),
        ],
        &[
            ("cloudflare/@cf/zai-org/glm-5.2", "cloudflare"),
            ("novita/zai-org/glm-5.2", "novita"),
        ],
    );
    // "cloudflare/..." sorts before "novita/...", so its output rate must win.
    assert_eq!(t.rate("glm-5.2").map(|r| r.output), Some(5.6e-6));
}

#[test]
fn rate_prefers_official_over_cheaper_reseller() {
    // openrouter lists glm-4.7 cheaper, but step (e) hits 'zai/glm-4.7' first.
    let t = table_pv(
        &[
            ("zai/glm-4.7", 7e-7, 2.8e-6, 0.0, 0.0),
            ("openrouter/z-ai/glm-4.7", 5e-7, 1e-6, 0.0, 0.0),
        ],
        &[
            ("zai/glm-4.7", "zai"),
            ("openrouter/z-ai/glm-4.7", "openrouter"),
        ],
    );
    assert_eq!(t.rate("glm-4.7").map(|r| r.input), Some(7e-7));
}

#[test]
fn rate_dotted_rewrite_to_official() {
    // 'zai.glm-4.7' → 'zai/glm-4.7' (not the feed's bedrock_converse entry).
    let t = table(&[("zai/glm-4.7", 7e-7, 2.8e-6, 0.0, 0.0)]);
    assert_eq!(t.rate("zai.glm-4.7").map(|r| r.input), Some(7e-7));
}

#[test]
fn rate_colon_strip_namespaced() {
    // 'zai/glm-4.7:free' → colon strip → exact 'zai/glm-4.7'.
    let t = table(&[
        ("zai/glm-4.7", 7e-7, 2.8e-6, 0.0, 0.0),
        ("deepseek/deepseek-v3.2", 2.8e-7, 4.2e-7, 0.0, 0.0),
    ]);
    assert_eq!(t.rate("zai/glm-4.7:free").map(|r| r.input), Some(7e-7));
    assert_eq!(
        t.rate("deepseek/deepseek-v3.2:free").map(|r| r.input),
        Some(2.8e-7)
    );
}

#[test]
fn rate_only_reseller_entries_returns_none() {
    // Only reseller entries exist; no official key, no non-reseller org-branded.
    let t = table_pv(
        &[("openrouter/z-ai/glm-4.7", 5e-7, 1e-6, 0.0, 0.0)],
        &[("openrouter/z-ai/glm-4.7", "openrouter")],
    );
    assert!(t.rate("glm-4.7").is_none());
}

#[test]
fn cache_file_without_providers_parses() {
    // An old cache written before the provider field existed must still load.
    let json = r#"{
        "fetched_at_ms": 12345,
        "rates": {
            "claude-opus-4-8": {
                "input": 0.000005,
                "output": 0.000025,
                "cache_read": 0.0000005,
                "cache_write": 0.00000625
            }
        }
    }"#;
    let cache: CacheFile = serde_json::from_str(json).expect("old cache parses");
    assert_eq!(cache.fetched_at_ms, 12345);
    assert!(cache.providers.is_empty());
    assert!(cache.rates.contains_key("claude-opus-4-8"));
}

// ── cost ───────────────────────────────────────────────────────────────────

#[test]
fn cost_sums_all_four_buckets() {
    // Clean rates: $1/$2/$0.10/$1.25 per million.
    let t = table(&[("m", 1e-6, 2e-6, 1e-7, 1.25e-6)]);
    let m = model("m", 1_000_000, 1_000_000, 1_000_000, 1_000_000);
    // 1.0 + 2.0 + 0.10 + 1.25 = 4.35
    let c = t.cost(&m).expect("priced");
    assert!((c - 4.35).abs() < 1e-9, "got {c}");
}

#[test]
fn cost_none_for_unpriced_model() {
    let t = table(&[("m", 1e-6, 2e-6, 1e-7, 1.25e-6)]);
    assert!(t.cost(&model("unknown", 1000, 0, 0, 0)).is_none());
}

#[test]
fn total_cost_counts_unpriced_with_tokens() {
    let t = table(&[("m", 1e-6, 2e-6, 0.0, 0.0)]);
    let models = vec![
        model("m", 1_000_000, 0, 0, 0),     // $1.00, priced
        model("unknown", 500_000, 0, 0, 0), // unpriced, has tokens → counted
        model("empty-unknown", 0, 0, 0, 0), // unpriced, no tokens → ignored
    ];
    let (total, unpriced) = t.total_cost(&models);
    assert!((total - 1.0).abs() < 1e-9, "got {total}");
    assert_eq!(unpriced, 1);
}
