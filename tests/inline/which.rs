#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::BTreeMap;

use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, ProfileName};

fn oauth_profile(name: &str, refresh: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: format!("at-{name}"),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn endpoint_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: Some("https://example.test".to_string()),
        api_key: Some("sk-x".to_string()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn blank_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn live_oauth(refresh: Option<&str>) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-live".to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

fn config_with(profiles: Vec<Profile>, active: Option<&str>) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            ..Default::default()
        },
        profiles,
    }
}

#[test]
fn matches_profile_by_refresh_token() {
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            oauth_profile("personal", "rt-personal"),
        ],
        Some("work"),
    );
    assert_eq!(
        match_by_refresh_token(&config, "rt-personal"),
        Some("personal")
    );
}

#[test]
fn returns_none_when_no_profile_holds_token() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    assert_eq!(match_by_refresh_token(&config, "rt-stranger"), None);
}

#[test]
fn ties_break_on_active_profile() {
    // degenerate: duplicate profile dir gives two profiles the same token; active wins
    let config = config_with(
        vec![
            oauth_profile("first", "rt-shared"),
            oauth_profile("second", "rt-shared"),
        ],
        Some("second"),
    );
    assert_eq!(match_by_refresh_token(&config, "rt-shared"), Some("second"));
}

#[test]
fn endpoint_profiles_without_oauth_are_skipped() {
    let config = config_with(
        vec![endpoint_profile("api"), oauth_profile("work", "rt-work")],
        None,
    );
    assert_eq!(match_by_refresh_token(&config, "rt-work"), Some("work"));
}

#[test]
fn attributes_unmatched_login_to_credential_less_active() {
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("new")],
        Some("new"),
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None),
        Some(("new", Source::CredentialLessActive))
    );
}

#[test]
fn token_match_wins_over_credential_less_active() {
    let config = config_with(
        vec![
            oauth_profile("personal", "rt-personal"),
            blank_profile("new"),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-personal"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None),
        Some(("personal", Source::RefreshMatch))
    );
}

#[test]
fn no_attribution_when_active_profile_has_creds() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

#[test]
fn no_attribution_when_no_active_profile() {
    let config = config_with(vec![blank_profile("new")], None);
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

#[test]
fn attributes_credential_less_active_without_loaded_refresh_token() {
    // active credential-less profile owns the session even when the loaded
    // file carries no refresh token (API-key/endpoint auth carries none).
    let config = config_with(vec![blank_profile("new")], Some("new"));
    let live = live_oauth(None);
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None),
        Some(("new", Source::CredentialLessActive))
    );
}

#[test]
fn attributes_api_key_active_when_credentials_file_absent() {
    // switching to an API-key profile deletes ~/.claude/.credentials.json, so
    // the loaded creds are `None`. the active profile still owns the session.
    let config = config_with(vec![endpoint_profile("api")], Some("api"));
    assert_eq!(
        resolve_profile(&config, None, false, None),
        Some(("api", Source::CredentialLessActive))
    );
}

#[test]
fn no_credential_less_attribution_inside_session() {
    // inside a session (CLAUDE_CONFIG_DIR set), creds belong to the runtime profile —
    // suppress attribution so a credential-less active isn't incorrectly credited
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("active")],
        Some("active"),
    );
    let live = live_oauth(Some("rt-from-runtime"));
    assert_eq!(resolve_profile(&config, Some(&live), true, None), None);
}

#[test]
fn token_match_still_works_inside_session() {
    // token-exact match is always valid, even inside a session
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("active")],
        Some("active"),
    );
    let live = live_oauth(Some("rt-work"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, None),
        Some(("work", Source::RefreshMatch))
    );
}

#[test]
fn resolves_started_profile_in_runtime_session() {
    // `clauth start <blank>`: credential-less started profile owns the runtime session
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("new")],
        Some("work"),
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("new")),
        Some(("new", Source::SessionDir))
    );
}

#[test]
fn started_profile_resolves_with_no_loaded_creds() {
    // no creds yet (pre-first-login) — started profile still owns the session
    let config = config_with(vec![blank_profile("new")], Some("work"));
    assert_eq!(
        resolve_profile(&config, None, true, Some("new")),
        Some(("new", Source::SessionDir))
    );
}

#[test]
fn token_match_wins_over_started_profile() {
    // token match is more precise than path-derived profile
    let config = config_with(
        vec![
            oauth_profile("personal", "rt-personal"),
            blank_profile("new"),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-personal"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("new")),
        Some(("personal", Source::RefreshMatch))
    );
}

#[test]
fn unknown_started_profile_is_not_resolved() {
    // profile no longer exists → falls through to in-session suppression, no invented match
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("ghost")),
        None
    );
}

#[test]
fn disabled_profile_is_never_resolved_even_on_a_stale_token_match() {
    // A disabled profile's stored creds are left on disk untouched (disable
    // only flips the flag), so a stale live file that still matches its
    // refresh token must NOT surface it — disabled accounts are invisible to
    // `which` regardless of which resolution tier would otherwise match.
    let mut disabled = oauth_profile("acme", "rt-acme");
    disabled.disabled = true;
    let config = config_with(vec![disabled], None);
    let live = live_oauth(Some("rt-acme"));
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

#[test]
fn disabled_profile_is_never_resolved_as_credential_less_active() {
    // Belt-and-suspenders: even if a disabled profile were somehow still the
    // active one (a pre-existing on-disk state from before this gate
    // existed), `which` must not attribute the session to it.
    let mut disabled = blank_profile("acme");
    disabled.disabled = true;
    let config = config_with(vec![disabled], Some("acme"));
    let live = live_oauth(None);
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

/// `which --json`'s `tier` is `null` when nothing on disk claims a tier, which
/// is what `status.json` and the MCP tools already emit for the same account —
/// the bare "Claude" this field used to print was a plan the account never had,
/// and it made the three surfaces disagree.
#[test]
fn json_tier_is_null_when_no_tier_is_known() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let resolved = ("work".to_string(), Source::RefreshMatch);
    let value = json_view(&config, Some(&resolved));

    assert_eq!(
        value["profile"], "work",
        "fixture control: profile resolved"
    );
    assert!(
        value["tier"].is_null(),
        "tier must be null with no fetched plan and no token claim, got {}",
        value["tier"]
    );
}

/// The other direction: a token that DOES claim a tier still renders it.
#[test]
fn json_tier_renders_a_known_tier() {
    let mut profile = oauth_profile("work", "rt-work");
    if let Some(oauth) = profile
        .credentials
        .as_mut()
        .and_then(|c| c.claude_ai_oauth.as_mut())
    {
        oauth.subscription_type = Some("max".to_string());
    }
    let config = config_with(vec![profile], Some("work"));
    let resolved = ("work".to_string(), Source::RefreshMatch);

    assert_eq!(json_view(&config, Some(&resolved))["tier"], "Claude Max");
}

/// An unresolved session emits every field as `null` rather than dropping them,
/// so a consumer's key lookup never has to branch on presence.
#[test]
fn json_tier_is_null_when_nothing_resolved() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let value = json_view(&config, None);

    assert!(value["profile"].is_null());
    assert!(value["tier"].is_null());
}

#[test]
fn source_maps_to_wire_strings() {
    assert_eq!(Source::RefreshMatch.as_str(), "refresh_match");
    assert_eq!(Source::SessionDir.as_str(), "session_dir");
    assert_eq!(
        Source::CredentialLessActive.as_str(),
        "credential_less_active"
    );
}

/// Tier 2 keys on the config dir's NAME, so per-session runtime dirs have to
/// resolve too — otherwise `clauth which` and `session_auth` stop recognizing
/// every `clauth start` session, with nothing failing loudly. The legacy
/// unsuffixed path must keep resolving alongside it.
#[test]
fn session_profile_extracted_from_runtime_path() {
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new(
            "/home/u/.clauth/profiles/work/runtime"
        )),
        Some("work".to_string())
    );
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new(
            "/home/u/.clauth/profiles/work/runtime-4242-0"
        )),
        Some("work".to_string())
    );
}

#[test]
fn session_profile_none_for_non_runtime_path() {
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new("/home/u/.claude")),
        None
    );
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new("/home/u/.clauth/profiles/work")),
        None
    );
    // The isolated flavor was never attributable through this tier; widening the
    // name check must not start attributing it.
    for isolated in [
        "/home/u/.clauth/profiles/work/runtime-isolated",
        "/home/u/.clauth/profiles/work/runtime-isolated-4242-0",
    ] {
        assert_eq!(
            session_profile_from_config_dir(std::path::Path::new(isolated)),
            None,
            "{isolated} must not resolve to a profile"
        );
    }
}

/// `CLAUDE_CONFIG_DIR` describes the process ASKING, so it is the wrong input for
/// attributing another process's credentials. A TUI running inside a `clauth
/// start` session would otherwise claim every bare `claude` on the box for its
/// own runtime profile.
#[test]
fn resolve_global_ignores_claude_config_dir_in_the_readers_env() {
    let home = crate::testutil::HomeSandbox::new();
    let config = config_with(
        vec![blank_profile("global"), blank_profile("started")],
        Some("global"),
    );
    let runtime_dir = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("started")
        .join("runtime-4242-0");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&home, &runtime_dir);

    assert_eq!(
        resolve_active(&config),
        Some(("started".to_string(), Source::SessionDir)),
        "fixture control: the reader's own env attributes it to its runtime profile"
    );
    assert_eq!(
        resolve_global(&config),
        Some(("global".to_string(), Source::CredentialLessActive)),
        "the global credential link's owner does not depend on who is asking"
    );
}
