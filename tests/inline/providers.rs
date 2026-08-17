//! Inline tests for `crate::providers` — provider URL matching and the
//! disk-cache roundtrip. DeepSeek-specific mapping tests live in
//! `providers_deepseek.rs`.

use super::*;

// ── Provider::from_base_url ───────────────────────────────────────────────────

#[test]
fn deepseek_matches_exact_base_url() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_matches_base_url_with_path() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com/v1"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_rejects_host_extension() {
    // A bare prefix match would claim these and send the profile's API key
    // to the real provider endpoint.
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com.evil.tld"),
        None
    );
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.community"),
        None
    );
}

#[test]
fn a_userinfo_authority_is_not_the_provider_it_is_prefixed_with() {
    // `https://api.deepseek.com:443@evil.tld` has host `evil.tld` — everything
    // before the `@` is userinfo. Claiming it as DeepSeek pointed the typed
    // usage fetch at the real `api.deepseek.com` with this account's key, which
    // is the same leak the host-extension case above exists to stop, reached
    // through the port delimiter instead of the dot.
    for url in [
        "https://api.deepseek.com:443@evil.tld/v1",
        "https://api.deepseek.com:x@evil.tld",
        "https://api.deepseek.com:@evil.tld",
        "https://api.z.ai:8443@evil.tld",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com:1@evil.tld/apps/anthropic",
    ] {
        assert_eq!(Provider::from_base_url(url), None, "{url}");
    }

    // The positive leg: a real port, an empty port, and a port with a path all
    // still resolve, so the guard rejects userinfo rather than rejecting `:`.
    for url in [
        "https://api.deepseek.com:443",
        "https://api.deepseek.com:443/v1",
        "https://api.deepseek.com:/v1",
        "https://api.deepseek.com:?k=v",
    ] {
        assert_eq!(
            Provider::from_base_url(url),
            Some(Provider::DeepSeek),
            "{url}"
        );
    }
}

#[test]
fn deepseek_rejects_plain_http_and_unrelated_hosts() {
    assert_eq!(Provider::from_base_url("http://api.deepseek.com"), None);
    assert_eq!(Provider::from_base_url("https://api.anthropic.com"), None);
    assert_eq!(Provider::from_base_url(""), None);
}

#[test]
fn deepseek_matches_uppercase_host() {
    // Hosts are case-insensitive (RFC 3986) — a profile pasted with caps still
    // resolves to the provider rather than falling through to "plain API".
    assert_eq!(
        Provider::from_base_url("https://API.DeepSeek.com/v1"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_matches_explicit_port() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com:443/v1"),
        Some(Provider::DeepSeek)
    );
}

// ── Disk cache ────────────────────────────────────────────────────────────────

use crate::testutil::HomeSandbox;

#[test]
fn disk_cache_roundtrips_stats() {
    let _home = HomeSandbox::new();
    let stats = ThirdPartyStats {
        is_available: true,
        rows: vec![StatRow {
            label: "total".to_string(),
            value: "110.00 USD".to_string(),
            kind: StatRowKind::Body,
        }],
        bars: Vec::new(),
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    crate::profile_cache::write_profile_cache(
        "tp-cache-test",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats,
    );
    let loaded = crate::profile_cache::load_profile_cache::<ThirdPartyStats>(
        "tp-cache-test",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("cache present");
    assert!(loaded.is_available);
    assert_eq!(loaded.rows.len(), 1);
    assert_eq!(loaded.rows[0].value, "110.00 USD");
}

#[test]
fn disk_cache_missing_reads_as_none() {
    let _home = HomeSandbox::new();
    assert!(
        crate::profile_cache::load_profile_cache::<ThirdPartyStats>(
            "tp-cache-absent",
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        )
        .is_none()
    );
}

// ── api_origin ───────────────────────────────────────────────────────────────

#[test]
fn api_origin_strips_path_to_scheme_host() {
    assert_eq!(
        api_origin("https://api.z.ai/api/anthropic").as_deref(),
        Some("https://api.z.ai")
    );
    assert_eq!(
        api_origin("https://api.deepseek.com/v1").as_deref(),
        Some("https://api.deepseek.com")
    );
}

#[test]
fn api_origin_keeps_port_drops_query_and_fragment() {
    assert_eq!(
        api_origin("https://host.example:8443/path?x=1#frag").as_deref(),
        Some("https://host.example:8443")
    );
}

#[test]
fn api_origin_none_without_scheme_delimiter() {
    assert!(api_origin("api.z.ai/usage").is_none());
}

// ── ThirdPartyTarget::throttle_key ─────────────────────────────────────────────

#[test]
fn throttle_key_known_provider_uses_canonical_origin() {
    // Distinct providers key distinct hosts so they pace independently.
    assert_eq!(
        ThirdPartyTarget::Known {
            provider: Provider::DeepSeek,
            console: None,
        }
        .throttle_key(),
        "https://api.deepseek.com"
    );
    assert_eq!(
        ThirdPartyTarget::Known {
            provider: Provider::Zai,
            console: None,
        }
        .throttle_key(),
        "https://api.z.ai"
    );
    assert_eq!(
        ThirdPartyTarget::Known {
            provider: Provider::OpenRouter,
            console: None,
        }
        .throttle_key(),
        "https://openrouter.ai"
    );
}

#[test]
fn throttle_key_generic_strips_to_origin() {
    // Two api-key profiles on the same host collapse to one pacing key (serialize);
    // a different host yields a different key (parallel).
    assert_eq!(
        ThirdPartyTarget::Generic {
            base_url: "https://proxy.example/v1".to_string(),
        }
        .throttle_key(),
        "https://proxy.example"
    );
}

#[test]
fn throttle_key_generic_falls_back_to_raw_when_schemeless() {
    // No `://` to parse an origin from — the raw base URL is still a stable key.
    assert_eq!(
        ThirdPartyTarget::Generic {
            base_url: "localhost:1234".to_string(),
        }
        .throttle_key(),
        "localhost:1234"
    );
}

// ── Provider::console_url ─────────────────────────────────────────────────────

#[test]
fn console_url_is_the_vendor_page_per_provider() {
    // Exact values: a typo here sends an operator to the wrong product's console.
    assert_eq!(
        Provider::DeepSeek.console_url("https://api.deepseek.com"),
        Some("https://platform.deepseek.com/api_keys")
    );
    assert_eq!(
        Provider::Zai.console_url("https://api.z.ai/api/anthropic"),
        Some("https://z.ai/manage-apikey/apikey-list")
    );
    assert_eq!(
        Provider::OpenRouter.console_url("https://openrouter.ai/api"),
        Some("https://openrouter.ai/settings/keys")
    );
}

#[test]
fn console_url_answers_none_for_a_base_url_the_provider_does_not_own() {
    // The mismatched pair a caller could build by hand. Opening some other
    // account's console would be worse than offering nothing, and the
    // single-page providers are exactly where a wrong page reads as harmless —
    // so every arm re-checks, not just Alibaba's.
    assert_eq!(
        Provider::Alibaba.console_url("https://api.deepseek.com"),
        None
    );
    assert_eq!(
        Provider::OpenRouter.console_url("https://api.deepseek.com"),
        None
    );
    assert_eq!(Provider::DeepSeek.console_url("https://api.z.ai"), None);
    assert_eq!(
        Provider::Zai.console_url("https://token-plan.ap-southeast-1.maas.aliyuncs.com"),
        None
    );
    assert_eq!(Provider::DeepSeek.console_url(""), None);
}
