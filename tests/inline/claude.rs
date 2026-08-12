use super::*;
use crate::profile::{ClaudeCredentials, OAuthToken};
use std::fs;

fn creds(access: &str, refresh: Option<&str>) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

#[test]
fn diverged_returns_false_when_either_side_missing() {
    let c = creds("a", Some("r"));
    assert!(!credentials_diverged(None, Some(&c)));
    assert!(!credentials_diverged(Some(&c), None));
    assert!(!credentials_diverged(None, None));
}

#[test]
fn diverged_returns_false_when_tokens_match() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-1"));
    assert!(!credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_access_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-2", Some("refresh-1"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-2"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_disappears() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", None);
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_false_when_oauth_block_missing_on_one_side() {
    let with = creds("a", Some("r"));
    let without = ClaudeCredentials {
        claude_ai_oauth: None,
    };
    assert!(!credentials_diverged(Some(&with), Some(&without)));
    assert!(!credentials_diverged(Some(&without), Some(&with)));
}

#[test]
fn classify_link_missing_when_path_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Missing,
    );
}

#[test]
fn classify_link_diverged_when_plain_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&link, b"{}").expect("write live");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

/// macOS reality: Claude Code rewrites `~/.claude/.credentials.json` as a plain-file
/// mirror of the Keychain after every run, replacing clauth's symlink. When the live
/// token still matches the active profile's stored token, that is NOT divergence —
/// classify must report LinkedTo so an ordinary switch doesn't falsely prompt to
/// capture credentials that already match. (Regression: the switch prompt fired on
/// every `clauth <name>` because a plain file was unconditionally Diverged.)
#[test]
fn classify_link_linked_to_when_plain_file_token_matches_stored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let same = serde_json::to_vec(&creds("same-access", Some("same-refresh"))).expect("ser");
    fs::write(&link, &same).expect("write live");
    fs::write(&expected, &same).expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
        "a plain file whose token matches the profile is CC's mirror, not divergence",
    );
}

/// A plain file whose access token DIFFERS from the profile's stored token is a
/// genuine CC re-login / rotation — still Diverged so the capture prompt fires.
#[test]
fn classify_link_diverged_when_plain_file_token_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("live-access", Some("r"))).expect("ser"),
    )
    .expect("write live");
    fs::write(
        &expected,
        serde_json::to_vec(&creds("stored-access", Some("r"))).expect("ser"),
    )
    .expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

/// A degenerate empty access token on both sides is a corrupt/partial write, not
/// a completed login — it must NOT read as `LinkedTo` just because two empty
/// strings compare equal. Matches the completed-login intent of `is_first_login`.
#[test]
fn classify_link_diverged_when_plain_file_access_token_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let empty = serde_json::to_vec(&creds("", Some("r"))).expect("ser");
    fs::write(&link, &empty).expect("write live");
    fs::write(&expected, &empty).expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
        "an empty access token is not a completed login, so it is not a mirror",
    );
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_when_pointing_at_expected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}

#[cfg(unix)]
#[test]
fn classify_link_diverged_when_symlink_points_elsewhere() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let other = tmp.path().join("other.json");
    fs::write(&other, b"{}").expect("write other");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&other, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

#[test]
fn first_login_true_when_no_stored_creds_and_plain_oauth_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    assert!(is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_stored_creds_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    fs::write(&expected, b"{}").expect("write stored");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_link_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_oauth_block_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    // valid JSON but no OAuth block — mid-flight partial write
    fs::write(&link, b"{}").expect("write");
    assert!(!is_first_login_at(&link, &expected));
}

/// A logged-out CC shell keeps `claudeAiOauth` (just with blanked tokens) plus
/// unrelated keys like `mcpOAuth` — it must NOT classify as a first login, or
/// `adopt_first_login` deletes the live file (no install source to relink a
/// blank profile back to) and `mcpOAuth` is lost with it. Regression for the
/// gap PR #46's shell-awareness left in `is_first_login_at` specifically.
#[test]
fn first_login_false_when_live_is_a_logged_out_shell() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "",
                "refreshToken": null,
                "expiresAt": 0,
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write shell");
    assert!(!is_first_login_at(&link, &expected));
}

/// Companion to the shell case above, same seam: a completed login (non-blank
/// access token) with the same foreign `mcpOAuth` key still classifies as a
/// first login, so the shell fix can't over-correct and strand a real login.
#[test]
fn first_login_true_when_live_is_a_completed_login_with_foreign_keys() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "real-access",
                "refreshToken": "real-refresh",
                "expiresAt": 1_700_000_000_000_i64,
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write completed login");
    assert!(is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn first_login_false_when_link_is_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let store = tmp.path().join("store.json");
    fs::write(
        &store,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    std::os::unix::fs::symlink(&store, &link).expect("symlink");
    assert!(!is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_even_when_target_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    // target absent (e.g. first-ever link, before save_profile writes it)
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}

// ── account-change `[Y/n]` overwrite path ──────────────────────────────────
//
// When Claude Code re-logged into a different account while clauth was closed,
// the live `~/.claude/.credentials.json` is a plain file diverging from the
// active profile's stored chain. clauth shows a `[Y/n]` prompt before the
// stored tokens are overwritten. These tests pin the prompt's GATE (when it
// fires) and both BRANCHES (confirm overwrites/captures, cancel is a no-op) at
// the home-derived seam the prompt actually drives, no TTY needed.

// Not `#[cfg(unix)]`: the ungated session-token tests below use HomeSandbox on
// every platform (it writes only a tempdir + files, no symlinks), so gating the
// import broke the Windows test build.
use crate::testutil::HomeSandbox;

/// Seed an active profile `name` with stored credentials, then simulate CC
/// re-logging into a different account: write a plain (non-symlink) live
/// `~/.claude/.credentials.json` carrying `live`. Returns the assembled config.
// Not `#[cfg(unix)]`: writes only plain files, and the ungated session-token
// tests call it on Windows too.
fn seed_relogin_scenario(
    name: &str,
    stored: ClaudeCredentials,
    live: ClaudeCredentials,
) -> AppConfig {
    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.credentials = Some(stored);
    crate::profile::save_profile(&profile).expect("save profile");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser live")).expect("write live");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some(name.into());
    config.state.profiles = vec![name.into()];
    config
}

/// The `[Y/n]` prompt's gate: a re-login is a Diverged plain file that is NOT a
/// first login (the profile already has stored creds), so the prompt fires.
#[cfg(unix)]
#[test]
fn relogin_is_diverged_and_not_first_login() {
    let _home = HomeSandbox::new();
    let _config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::Diverged,
        "a CC re-login leaves a plain file diverging from the stored chain",
    );
    assert!(
        !is_first_login("active").expect("first login"),
        "stored creds exist, so this is a re-login overwrite, not a first login",
    );
}

/// Confirm branch (`y`): capture the live re-login into the active profile, then
/// relink. The stored chain is overwritten with the live one and the live path
/// becomes a symlink back to the profile's now-updated credentials.
#[cfg(unix)]
#[test]
fn overwrite_confirm_captures_relogin_into_profile() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    // `y` answer = force-snapshot the live creds into the active profile, relink.
    force_snapshot_active_credentials(&mut config).expect("snapshot");
    force_link_profile_credentials("active").expect("relink");

    // The profile's stored chain now holds the re-logged tokens.
    let stored = config
        .find("active")
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.refresh_token());
    assert_eq!(
        stored,
        Some("relogin-refresh"),
        "confirm must overwrite the stored chain with the live re-login",
    );

    // The live path is reconciled back to a symlink into the profile.
    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::LinkedTo,
        "after capture+relink the live path links to the profile's creds",
    );

    // The on-disk profile credentials file carries the re-logged chain too.
    let on_disk: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("active")
            .expect("profile dir")
            .join("credentials.json"),
    )
    .expect("read stored creds");
    assert_eq!(
        on_disk.refresh_token(),
        Some("relogin-refresh"),
        "the persisted profile credentials must hold the captured chain",
    );
}

/// Cancel branch (`n`): no capture, no relink. The stored chain keeps its old
/// tokens and the live path is left exactly as CC wrote it (untouched).
#[cfg(unix)]
#[test]
fn overwrite_cancel_leaves_stored_and_live_untouched() {
    let _home = HomeSandbox::new();
    let config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    // `n` answer = abort. We perform no snapshot and no relink; assert the
    // pre-prompt state is preserved.
    let stored = config
        .find("active")
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.refresh_token());
    assert_eq!(
        stored,
        Some("stored-refresh"),
        "cancel must not overwrite the stored chain",
    );

    // The live file CC wrote is still a plain diverged file with its own chain.
    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::Diverged,
        "cancel leaves the live re-login in place (still diverged)",
    );
    let live = read_claude_credentials()
        .expect("read live")
        .expect("live present");
    assert_eq!(
        live.refresh_token(),
        Some("relogin-refresh"),
        "cancel must leave the live re-login bytes untouched",
    );
}

#[test]
fn build_settings_writes_model_knobs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json"); // absent → starts from `{}`
    let mut profile = crate::profile::Profile::new("p".to_string(), None, None);
    profile.models = crate::profile::ModelSettings {
        default: Some("opusplan".to_string()),
        opus: Some("claude-opus-4-8[1m]".to_string()),
        sonnet: None,
        haiku: None,
        fable: Some("claude-fable-5".to_string()),
        subagent: Some("claude-haiku-4-5".to_string()),
    };
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert_eq!(v["model"], "opusplan", "default model → top-level `model`");
    assert_eq!(
        v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
        "claude-opus-4-8[1m]"
    );
    assert_eq!(v["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"], "claude-fable-5");
    assert_eq!(v["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "claude-haiku-4-5");
    assert!(
        v["env"].get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none(),
        "an unset tier override writes no env key",
    );
}

/// `ModelSettings::is_empty` is the gate that decides whether a profile with no
/// endpoint and no env is worth writing settings for at all, so a tier missing
/// from it makes that tier's ONLY-set case a silent no-write.
#[test]
fn a_tier_override_alone_is_enough_to_write_settings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json"); // absent → nothing to merge onto
    let mut profile = crate::profile::Profile::new("p".to_string(), None, None);
    profile.models.fable = Some("claude-fable-5".to_string());
    assert!(
        !profile.models.is_empty(),
        "a lone tier override is not an empty model block",
    );

    crate::profile::save_profile(&profile).expect("save profile");
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert_eq!(v["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"], "claude-fable-5");
}

// A profile with no model config must strip a previous profile's model knobs
// from the base settings.json, so a switch never inherits stale model routing.
#[test]
fn build_settings_clears_stale_model_knobs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    fs::write(
        &base,
        r#"{"model":"opus","env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"old","ANTHROPIC_DEFAULT_FABLE_MODEL":"old","CLAUDE_CODE_SUBAGENT_MODEL":"old","KEEP":"1"}}"#,
    )
    .expect("seed base settings");
    let profile = crate::profile::Profile::new("p".to_string(), None, None); // empty models
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert!(v.get("model").is_none(), "top-level `model` cleared");
    assert!(v["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none());
    assert!(v["env"].get("ANTHROPIC_DEFAULT_FABLE_MODEL").is_none());
    assert!(v["env"].get("CLAUDE_CODE_SUBAGENT_MODEL").is_none());
    assert_eq!(v["env"]["KEEP"], "1", "unrelated env keys are preserved");
}

// ── apiKeyHelper for api-key profiles ─────────────────────────────────────────
//
// `build_claude_settings_json` swaps `env.ANTHROPIC_AUTH_TOKEN` for CC's
// top-level `apiKeyHelper` when a profile carries an api_key, so the raw key
// leaves the settings.json `env` block and the spawned CC process's env. CC
// runs the helper per request and sends its stdout as both `X-Api-Key` and
// `Authorization: Bearer` (see `docs/security.md`).

/// An api-key profile writes `apiKeyHelper` at the top level (NOT under `env`),
/// keeps the raw key out of the rendered JSON, and clears `env.ANTHROPIC_AUTH_TOKEN`.
/// The helper string carries the live exe path, the hidden subcommand, and the
/// profile name — the three tokens CC's shell will re-split.
#[test]
fn build_settings_writes_api_key_helper_not_env_token() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json"); // absent → starts from `{}`
    let profile = crate::profile::Profile::new(
        "acme".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret-DO-NOT-LEAK".to_string()),
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");

    // Top-level `apiKeyHelper` (not nested under `env`).
    let helper = v
        .get("apiKeyHelper")
        .and_then(|h| h.as_str())
        .expect("apiKeyHelper must be a top-level string");
    assert!(
        v["env"].get("apiKeyHelper").is_none(),
        "apiKeyHelper must NOT live under `env` (CC reads it only at the top level)"
    );

    // The helper command carries the exe path, the hidden subcommand, and the
    // profile name — so CC's shell-invocation of clauth can re-derive the key.
    let exe = std::env::current_exe().expect("test-bin current_exe");
    let exe_str = exe.to_string_lossy();
    // Compared through `shell_quote`: on windows it escapes every `\`, so an
    // absolute exe path never appears literally in the helper.
    assert!(
        helper.contains(&shell_quote(&exe_str)),
        "helper ({helper}) must carry the quoted current exe path ({exe_str})"
    );
    assert!(
        helper.contains("__api-key"),
        "helper ({helper}) must carry the hidden subcommand name"
    );
    assert!(
        helper.contains("acme"),
        "helper ({helper}) must carry the profile name"
    );

    // The raw key MUST NOT appear anywhere in the rendered settings.json:
    // not in env, not at the top level, not inside the helper string.
    assert!(
        !json.contains("sk-secret-DO-NOT-LEAK"),
        "raw api_key must not appear in settings.json; got: {json}"
    );
    assert!(
        v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none(),
        "env.ANTHROPIC_AUTH_TOKEN must be absent (the helper replaces it)"
    );
}

/// A profile with no api_key (OAuth, local endpoint) writes NO `apiKeyHelper`
/// and NO `env.ANTHROPIC_AUTH_TOKEN` — bit-identical to the pre-helper stock
/// behavior. A switch from an api-key profile must clear both.
#[test]
fn build_settings_no_api_key_helper_for_non_api_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    // Seed the base with stale keys the way a prior api-key profile would leave
    // behind — the non-api rebuild must strip both.
    fs::write(
        &base,
        r#"{"apiKeyHelper":"/old/bin/helper","env":{"ANTHROPIC_AUTH_TOKEN":"stale","ANTHROPIC_BASE_URL":"https://api.example.com"}}"#,
    )
    .expect("seed base settings");
    // A non-api-key profile: OAuth/login shape. Carries the seeded base_url so
    // the rebuild preserves it (the assertion below pins that unrelated env
    // keys survive — base_url would otherwise be cleared by `match base_url`).
    let profile = crate::profile::Profile::new(
        "p".to_string(),
        Some("https://api.example.com".to_string()),
        None,
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");

    assert!(
        v.get("apiKeyHelper").is_none(),
        "non-api profile must not write apiKeyHelper; got: {json}"
    );
    assert!(
        v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none(),
        "non-api profile must clear env.ANTHROPIC_AUTH_TOKEN"
    );
    // Unrelated base settings survive.
    assert_eq!(
        v["env"]["ANTHROPIC_BASE_URL"], "https://api.example.com",
        "unrelated env keys are preserved"
    );
}

/// Switching from an api-key profile to a base_url-only profile (no api_key)
/// must drop `apiKeyHelper` and `env.ANTHROPIC_AUTH_TOKEN` together — a stale
/// helper pointing at the old profile would route the new session's requests
/// through the old account.
#[test]
fn build_settings_switch_away_from_api_key_clears_helper() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    fs::write(
        &base,
        r#"{"apiKeyHelper":"/old/clauth __api-key oldacct","env":{"ANTHROPIC_AUTH_TOKEN":"sk-old","ANTHROPIC_BASE_URL":"https://old.example.com"}}"#,
    )
    .expect("seed api-key base settings");
    let profile = crate::profile::Profile::new(
        "new".to_string(),
        Some("https://new.example.com".to_string()),
        None,
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert!(v.get("apiKeyHelper").is_none());
    assert!(v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://new.example.com");
}

/// The helper command string shell-quotes a spaces-in-path exe so the system
/// shell re-splits it into three tokens. Unix-only because the quoter branches
/// on `cfg(unix)`; Windows quoting is covered structurally (it wraps the same
/// way) but cmd's grammar is too ambiguous to assert byte-exact.
#[test]
fn build_settings_api_key_helper_shell_quotes_exe_path() {
    #[cfg(unix)]
    {
        let quoted = shell_quote("/home/uwu clxdy/bin/clauth");
        // POSIX single-quote, with `'` inside escaped as `'\''`.
        assert_eq!(quoted, "'/home/uwu clxdy/bin/clauth'");

        // A safe-char-only path (the cargo-installed default) is left unquoted.
        let safe = shell_quote("/home/uwuclxdy/.cargo/bin/clauth");
        assert_eq!(safe, "/home/uwuclxdy/.cargo/bin/clauth");

        // An embedded single-quote closes, escapes, and reopens the outer quote.
        let tricky = shell_quote("/path/with/'/clauth");
        assert_eq!(tricky, "'/path/with/'\\''/clauth'");
    }
    #[cfg(not(unix))]
    {
        // Non-Unix quoter is structurally similar but covered only on Windows
        // targets; this test exists for the positive-control assertion on Unix.
    }
}

/// Profile names are validated to a shell-safe charset, so the helper command
/// never needs to quote them. This pins the fast-path: a regression that
/// started escaping profile names would still pass the round-trip but would
/// drift from CC's documented `/bin/<script>` example shape.
#[test]
fn build_settings_api_key_helper_leaves_profile_name_unquoted() {
    let exe = std::path::Path::new("/usr/local/bin/clauth");
    let cmd = build_api_key_helper_command(exe, "acme_corp-1.0+@");
    assert_eq!(
        cmd, "/usr/local/bin/clauth __api-key acme_corp-1.0+@",
        "validated profile names must not be over-quoted"
    );
}

/// A long-lived process (daemon/TUI) that rebuilds settings after an in-place
/// self-update reads `env::current_exe()` as `<path> (deleted)` on Linux. The
/// helper strips that marker so CC execs the installed binary at the same path,
/// not a dead one — otherwise every mint 401s until a fresh process rebuilds.
#[test]
fn build_settings_api_key_helper_strips_deleted_exe_marker() {
    let exe = std::path::Path::new("/home/uwuclxdy/.cargo/bin/clauth (deleted)");
    let cmd = build_api_key_helper_command(exe, "acme");
    assert_eq!(cmd, "/home/uwuclxdy/.cargo/bin/clauth __api-key acme");
}

// ── profile_name_from_helper: structural parse of the helper command string ──
//
// `read_claude_endpoint_config` derives the live api_key by parsing the
// `apiKeyHelper` string the runtime settings.json carries. The parser must
// reject anything that isn't exactly `<exe> __api-key <profile>` — a
// hand-edited helper or a different command shape must NOT trigger a profile
// lookup, or `capture_snapshot` could pull the wrong account's key.

#[test]
fn profile_name_from_helper_parses_our_shape() {
    // The shape `build_api_key_helper_command` emits.
    assert_eq!(
        profile_name_from_helper("/usr/local/bin/clauth __api-key acme"),
        Some("acme".to_string()),
    );
    // Exe path with spaces is shell-quoted; split_whitespace still yields
    // three tokens.
    assert_eq!(
        profile_name_from_helper("'/home/uwu clxdy/bin/clauth' __api-key acme"),
        Some("acme".to_string()),
    );
    // Profile name with every validated charset char round-trips.
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key a_b.c@d+e-f"),
        Some("a_b.c@d+e-f".to_string()),
    );
}

#[test]
fn profile_name_from_helper_rejects_wrong_shape() {
    // Not enough tokens.
    assert_eq!(profile_name_from_helper("/x/clauth"), None);
    assert_eq!(profile_name_from_helper("/x/clauth __api-key"), None);
    assert_eq!(profile_name_from_helper(""), None);
    // Too many tokens — a future shape with flags after the name is NOT ours.
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key acme --flag"),
        None,
    );
    // Middle token isn't our subcommand name.
    assert_eq!(
        profile_name_from_helper("/custom/helper acme"),
        None,
        "a foreign helper must not trigger a profile lookup"
    );
    assert_eq!(
        profile_name_from_helper("/x/clauth __other-hidden-cmd acme"),
        None,
    );
    // Profile name fails `validate_profile_name`'s charset.
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key bad/name"),
        None,
        "a path-shaped third token must not parse as a profile name"
    );
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key .hidden"),
        None,
        "a leading-dot profile name is rejected by validate_profile_name"
    );
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key 'quoted'"),
        None,
        "a quoted profile name means it failed validate_profile_name's charset"
    );
}

/// A whitespace-only api_key is treated as absent at the build layer (matching
/// `api_key_for_profile`'s trim-and-filter at the helper end), so the helper
/// is NOT written for it and `cmd_api_key` will fail closed rather than mint
/// a blank credential.
#[test]
fn build_settings_blank_api_key_writes_no_helper() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    fs::write(&base, r#"{"apiKeyHelper":"/stale/bin/helper"}"#).expect("seed");
    let profile = crate::profile::Profile::new(
        "p".to_string(),
        Some("https://api.example.com".to_string()),
        Some("   ".to_string()),
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert!(
        v.get("apiKeyHelper").is_none(),
        "a whitespace-only api_key must clear the helper, not write one"
    );
    assert!(
        v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none(),
        "a whitespace-only api_key must not write the env var either"
    );
}

// ── logged-out shell detection ────────────────────────────────────────────────
//
// When Claude Code's own token refresh dies it does not delete the live
// `.credentials.json`: it blanks both tokens and zeroes `expiresAt`, keeping
// unrelated keys like `mcpOAuth` — a logged-out shell. A shell still
// classifies Diverged, so without the exemption every guard built on
// "diverged and unsaved" deferred switches behind a TUI decision about an
// empty file.

/// Truth table for [`live_login_is_empty`]: only a login with NO usable token
/// (both absent or blank, or no OAuth block at all) is empty — one live token
/// on either side keeps the login's protections.
#[test]
fn live_login_is_empty_truth_table() {
    // CC's logged-out shell: both tokens blanked.
    assert!(live_login_is_empty(&creds("", Some(""))));
    // Blank access token and no refresh token at all.
    assert!(live_login_is_empty(&creds("", None)));
    // No OAuth block (a file holding only foreign keys like mcpOAuth).
    assert!(live_login_is_empty(&ClaudeCredentials {
        claude_ai_oauth: None,
    }));
    // A live access token alone is a login.
    assert!(!live_login_is_empty(&creds("at-live", None)));
    assert!(!live_login_is_empty(&creds("at-live", Some(""))));
    // A refresh token alone is a login (the access side merely expired).
    assert!(!live_login_is_empty(&creds("", Some("rt-live"))));
    // A full pair is a login.
    assert!(!live_login_is_empty(&creds("at-live", Some("rt-live"))));
}

/// [`live_credentials_are_shell`] is true only for a PARSED empty login: a
/// missing file is not a shell, and an unreadable/non-JSON file is not a shell
/// either (it may be a CC write in progress — "possibly a login" must keep a
/// real login's protections).
#[test]
fn live_credentials_are_shell_requires_a_parsed_empty_login() {
    let _home = crate::testutil::HomeSandbox::new();
    let live = claude_credentials_path().expect("creds path");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");

    // Missing file: nothing there to call a shell.
    assert!(!live_credentials_are_shell());

    // CC's logged-out shell, foreign keys and all.
    fs::write(
        &live,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "",
                "refreshToken": "",
                "expiresAt": 0,
                "scopes": ["user:inference"],
                "subscriptionType": "max",
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write shell");
    assert!(live_credentials_are_shell());

    // No OAuth block at all is the same shell.
    fs::write(&live, r#"{"mcpOAuth":{}}"#).expect("write oauth-less file");
    assert!(live_credentials_are_shell());

    // Torn JSON (a write in progress): NOT a shell — guards stay armed.
    fs::write(&live, br#"{"claudeAiOauth":{"accessToken":""#).expect("write torn file");
    assert!(!live_credentials_are_shell());

    // A real login: not a shell.
    fs::write(
        &live,
        serde_json::to_vec(&creds("at-live", Some("rt-live"))).expect("ser live"),
    )
    .expect("write live");
    assert!(!live_credentials_are_shell());
}

/// `force_snapshot_active_credentials` is the shared sink `reconcile_startup`
/// reaches via `default_divergence: Overwrite` with no sibling owner — a
/// logged-out shell in the live slot must never overwrite the profile's real
/// stored login with blanks (recoverable only by re-login). The second half is
/// a positive control: the guard is narrow to shells only, so a REAL diverged
/// login is still captured by the same sink.
#[test]
fn force_snapshot_skips_shell_but_still_captures_real_divergence() {
    let _home = HomeSandbox::new();

    let mut shell_config = seed_relogin_scenario(
        "shell-active",
        creds("stored-access", Some("stored-refresh")),
        creds("", Some("")),
    );
    force_snapshot_active_credentials(&mut shell_config).expect("force snapshot shell");
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("shell-active")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.access_token(),
        Some("stored-access"),
        "a logged-out shell must never overwrite the stored access token",
    );
    assert_eq!(
        stored.refresh_token(),
        Some("stored-refresh"),
        "a logged-out shell must never overwrite the stored refresh token",
    );

    let mut real_config = seed_relogin_scenario(
        "real-active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );
    force_snapshot_active_credentials(&mut real_config).expect("force snapshot real");
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("real-active")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.access_token(),
        Some("relogin-access"),
        "a real diverged login must still be captured by the guard",
    );
    assert_eq!(
        stored.refresh_token(),
        Some("relogin-refresh"),
        "a real diverged login must still be captured by the guard",
    );
}

/// `reconcile_startup`'s non-diverged sink, `snapshot_active_credentials`,
/// used to route a blank (credential-less) active profile's shell-shaped live
/// file through `is_first_login` -> `adopt_first_login`, which deletes the
/// live file to relink it — but a blank profile has no install source, so
/// nothing gets relinked and the live file (with `mcpOAuth`) is simply gone.
/// The 1Hz poll and the divergence prompt both already guard their own adopt
/// call with `live_credentials_are_shell()`; this pins the startup sink to
/// the same behavior via the shared `is_first_login` classification.
#[test]
fn snapshot_skips_shell_on_blank_profile_and_preserves_live_file() {
    let _home = HomeSandbox::new();

    let profile = crate::profile::Profile::new("blank-active".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some("blank-active".into());
    config.state.profiles = vec!["blank-active".into()];

    let live = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    let shell_json = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "",
            "refreshToken": null,
            "expiresAt": 0,
        },
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    })
    .to_string();
    fs::write(&live, &shell_json).expect("write shell");

    snapshot_active_credentials(&mut config).expect("snapshot");

    assert!(
        live.exists(),
        "a logged-out shell must not be adopted as a first login, so the live file survives",
    );
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&live).expect("read live")).expect("parse");
    assert_eq!(
        after["mcpOAuth"]["some-server"]["accessToken"], "mcp-tok",
        "mcpOAuth must survive untouched — the sink never adopts, so it never rewrites the slot",
    );
    assert!(
        config
            .find("blank-active")
            .expect("profile")
            .credentials
            .is_none(),
        "nothing was adopted into the blank profile",
    );
}

/// Sibling hole to the shell case: a TOCTOU delete of the live file inside the
/// confirm window, or a dangling symlink, makes `read_claude_credentials`
/// return `Ok(None)`. That absence is not a login either — the sink must skip
/// the capture instead of wiping the stored login down to `None`.
#[test]
fn force_snapshot_skips_an_absent_live_file() {
    let _home = HomeSandbox::new();

    let mut profile = crate::profile::Profile::new("absent-active".to_string(), None, None);
    profile.credentials = Some(creds("stored-access", Some("stored-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some("absent-active".into());
    config.state.profiles = vec!["absent-active".into()];

    // No live `.credentials.json` written at all: `claude_credentials_path()`
    // does not exist, matching a TOCTOU delete or a dangling symlink.

    force_snapshot_active_credentials(&mut config).expect("force snapshot absent");

    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("absent-active")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.access_token(),
        Some("stored-access"),
        "an absent live file must never overwrite the stored access token",
    );
    assert_eq!(
        stored.refresh_token(),
        Some("stored-refresh"),
        "an absent live file must never overwrite the stored refresh token",
    );
}

// ── CLA-SPLIT: long-lived session token beside the usage OAuth pair ───────────

/// Write a `session-token.json` (static long-lived login) into `name`'s
/// profile dir, as the split-credential fill does.
fn fill_session_token_by_hand(name: &str, access: &str) {
    let dir = crate::profile::profile_dir(name).expect("profile dir");
    fs::create_dir_all(&dir).expect("mkdir profile");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds(access, None)).expect("ser session token"),
    )
    .expect("write session token");
}

/// The install source is `credentials.json` until a session token appears,
/// then the session token — and never the OAuth pair while it exists.
#[test]
fn install_source_prefers_session_token() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    assert!(!has_session_token("split"));
    assert!(
        install_source_path("split")
            .expect("source")
            .ends_with("credentials.json")
    );

    fill_session_token_by_hand("split", "oat-access");
    assert!(has_session_token("split"));
    assert!(
        install_source_path("split")
            .expect("source")
            .ends_with("session-token.json")
    );
}

/// `installed_session_token` answers with exactly the token a switch installs,
/// which is what `clauth which` attributes the live slot by. It has to track
/// `has_session_token`: a mis-filled sidecar (one carrying a refresh token) is
/// never installed, so attributing a profile by it would name an account no
/// session is running as.
#[test]
fn installed_session_token_tracks_what_a_switch_installs() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    assert_eq!(installed_session_token("split"), None, "no sidecar yet");

    fill_session_token_by_hand("split", "oat-access");
    assert_eq!(
        installed_session_token("split").as_deref(),
        Some("oat-access")
    );

    // Mis-fill: a rotating pair in the sidecar leaves the split disengaged, so
    // the install source is the OAuth pair and there is nothing to attribute by.
    let dir = crate::profile::profile_dir("split").expect("profile dir");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("oat-access", Some("rt-misfill"))).expect("ser sidecar"),
    )
    .expect("write sidecar");
    assert_eq!(
        session_token_status("split"),
        Some(SessionTokenStatus::NotLongLived)
    );
    assert_eq!(
        installed_session_token("split"),
        None,
        "mis-fill installs nothing"
    );

    // A blank access token is Claude Code's logged-out shell, not a login. It
    // must not become an attribution key, or every profile holding a blanked
    // sidecar would answer to the same empty string.
    fill_session_token_by_hand("split", "");
    assert!(
        has_session_token("split"),
        "a blank mint is still long-lived"
    );
    assert_eq!(
        installed_session_token("split"),
        None,
        "blank is not a token"
    );
}

/// Clearing the sidecar is the only exit from the split. It flips the install
/// source back to the OAuth pair, and it is idempotent: the second call reports
/// "nothing to clear" rather than failing, so a repeated `--clear` is harmless.
#[test]
fn clear_session_token_flips_the_install_source_back() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");
    fill_session_token_by_hand("split", "oat-access");
    assert!(
        install_source_path("split")
            .expect("source")
            .ends_with("session-token.json")
    );

    assert!(clear_session_token("split").expect("clear"), "removed one");
    assert!(!has_session_token("split"));
    assert_eq!(session_token_status("split"), None);
    assert!(
        install_source_path("split")
            .expect("source")
            .ends_with("credentials.json"),
        "install source flips back to the usage pair"
    );
    // The usage OAuth pair is untouched — clearing drops the sidecar only.
    assert_eq!(
        crate::profile::load_profile("split")
            .expect("reload")
            .credentials
            .and_then(|c| c.access_token().map(str::to_string))
            .as_deref(),
        Some("usage-access")
    );

    assert!(
        !clear_session_token("split").expect("second clear"),
        "idempotent: nothing left to remove"
    );
}

/// A live slot holding the profile's static session token is the designed
/// steady state: LinkedTo (the divergence machinery stays dormant), and a
/// snapshot leaves the clauth-private usage OAuth pair untouched instead of
/// clobbering it with the token just read.
#[test]
fn session_token_live_is_linked_and_snapshot_keeps_usage_oauth() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "split",
        creds("usage-access", Some("usage-refresh")),
        creds("oat-access", None),
    );
    fill_session_token_by_hand("split", "oat-access");

    assert_eq!(
        classify_credentials_link("split").expect("classify"),
        LinkState::LinkedTo,
        "live slot holding the session token is the steady state, not divergence",
    );

    snapshot_active_credentials(&mut config).expect("snapshot");
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("split")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh"),
        "snapshot must never overwrite the usage OAuth pair with the session token",
    );
}

/// A switch to a session-token profile links the LIVE slot to
/// `session-token.json` — the rotating usage pair is never installed, and it
/// survives the switch on disk byte-for-byte.
#[cfg(unix)]
#[test]
fn switch_installs_session_token_not_usage_oauth() {
    let _home = HomeSandbox::new();

    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("at-a", Some("rt-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("usage-access-b", Some("usage-refresh-b")));
    crate::profile::save_profile(&b).expect("save b");
    fill_session_token_by_hand("b", "oat-b");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![a, b],
    };
    config.state.profiles = vec!["a".into(), "b".into()];
    config.state.active_profile = Some("a".into());
    force_link_profile_credentials("a").expect("link a");

    crate::actions::switch_profile(&mut config, "b").expect("switch to b");

    let live_target =
        std::fs::read_link(claude_credentials_path().expect("path")).expect("live is a symlink");
    assert!(
        live_target.ends_with("session-token.json"),
        "the live slot must point at b's session token, got {live_target:?}",
    );
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("b")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read b store");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh-b"),
        "b's usage OAuth pair must survive the switch untouched",
    );
}

// ── CLA-SPLIT-2: the `--setup-token` capture flow's building blocks ───────────

/// The paste validator refuses everything but a clean single-token mint: a
/// broken sidecar signs every session out on first use, so the failure has to
/// happen at the paste, loudly, and without echoing the value.
#[test]
fn validate_setup_token_accepts_a_mint_and_rejects_bad_pastes() {
    let good = format!("sk-ant-oat01-{}", "x".repeat(48));
    assert_eq!(
        validate_setup_token(&format!("  {good}\n")).expect("valid"),
        good,
        "surrounding whitespace trims away"
    );
    assert!(validate_setup_token("").is_err(), "empty paste");
    assert!(validate_setup_token("   \n").is_err(), "blank paste");
    assert!(
        validate_setup_token("api-key-not-a-mint-0123456789012345678901234567890").is_err(),
        "wrong prefix"
    );
    assert!(
        validate_setup_token(&format!("Setup token: {good}")).is_err(),
        "paste with prompt text has interior whitespace"
    );
    assert!(
        validate_setup_token("sk-ant-short").is_err(),
        "truncated paste"
    );
    assert!(
        validate_setup_token(&format!("sk-ant-api03-{}", "z".repeat(48))).is_err(),
        "an API key must be rejected, not installed as the session bearer",
    );
}

/// The helper emits the api key verbatim to stdout, which CC forwards as an
/// `X-Api-Key`/`Authorization` header. An interior control char would inject or
/// malform that header, so a poisoned key must be refused, not minted.
#[test]
fn validate_api_key_rejects_control_and_whitespace() {
    assert!(
        validate_api_key("sk-ant-api03-abc123").is_ok(),
        "a clean key"
    );
    assert!(
        validate_api_key("sk-ant\r\nX-Evil: 1").is_err(),
        "CRLF injection"
    );
    assert!(validate_api_key("sk-ant\ndaemon").is_err(), "bare newline");
    assert!(validate_api_key("sk ant key").is_err(), "interior space");
    assert!(validate_api_key("sk-ant\tkey").is_err(), "tab");
    assert!(validate_api_key("sk-ant\u{0}key").is_err(), "nul");
}

/// Force-snapshot (the divergence-modal "overwrite" and the CLI reconciled
/// switch both reach it) must never capture the live login into a session-token
/// profile's clauth-private usage OAuth pair. Here the live slot holds a FOREIGN
/// login; the guard at the shared sink leaves the stored usage pair intact.
#[test]
fn force_snapshot_never_clobbers_the_session_token_usage_pair() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "split",
        creds("usage-access", Some("usage-refresh")),
        creds("foreign-access", Some("foreign-refresh")),
    );
    fill_session_token_by_hand("split", "oat-access");

    force_snapshot_active_credentials(&mut config).expect("force snapshot");

    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("split")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh"),
        "force-snapshot must leave the clauth-private usage OAuth pair untouched",
    );
}

/// The capture writes a sidecar the whole CLA-SPLIT machinery recognises:
/// `has_session_token` flips, the install source re-points, the stamped
/// one-year horizon reads back through `session_token_expiry`, and the file
/// carries credential permissions.
#[test]
fn write_session_token_produces_a_recognised_sidecar() {
    let _home = HomeSandbox::new();
    let profile = crate::profile::Profile::new("cap".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    assert_eq!(session_token_status("cap"), None, "no sidecar yet");

    let now = 1_700_000_000_000_i64;
    let token = format!("sk-ant-oat01-{}", "y".repeat(48));
    let stamped = write_session_token("cap", &token, now).expect("write sidecar");
    assert_eq!(stamped, now + SETUP_TOKEN_ASSUMED_LIFETIME_MS);

    assert!(has_session_token("cap"));
    assert!(
        install_source_path("cap")
            .expect("source")
            .ends_with("session-token.json")
    );
    assert_eq!(
        session_token_status("cap"),
        Some(SessionTokenStatus::LongLived(Some(stamped)))
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(
            crate::profile::profile_dir("cap")
                .expect("dir")
                .join("session-token.json"),
        )
        .expect("meta")
        .permissions()
        .mode();
        assert_eq!(mode & 0o777, 0o600, "sidecar is a credential file");
    }
}

/// A hand-rolled sidecar without `expiresAt` still reports "present, horizon
/// unknown" — never `None` (which would hide the token row entirely).
#[test]
fn session_token_status_distinguishes_missing_from_unstamped() {
    let _home = HomeSandbox::new();
    let profile = crate::profile::Profile::new("hand".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    fill_session_token_by_hand("hand", "oat-access");
    assert_eq!(
        session_token_status("hand"),
        Some(SessionTokenStatus::LongLived(None))
    );
}

// ── #53 review: the split engages only for a genuinely LONG-LIVED token ──────

/// A sidecar mis-filled with a rotating pair (refresh token present) must NOT
/// engage the split: it reads `NotLongLived`, `has_session_token` stays
/// false, and the install source falls back to `credentials.json` exactly as
/// if the sidecar weren't there — installing a dies-in-hours token with no
/// refresher behind it is the failure this detection exists to prevent.
#[test]
fn a_rotating_pair_in_the_sidecar_never_engages_the_split() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("mis".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let dir = crate::profile::profile_dir("mis").expect("profile dir");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("rotating-access", Some("rotating-refresh")))
            .expect("ser sidecar"),
    )
    .expect("write sidecar");

    assert_eq!(
        session_token_status("mis"),
        Some(SessionTokenStatus::NotLongLived)
    );
    assert!(!has_session_token("mis"), "the split stays disengaged");
    assert!(
        install_source_path("mis")
            .expect("source")
            .ends_with("credentials.json"),
        "switches keep installing the rotating pair from credentials.json"
    );
}

/// The macOS steady state, and the reason the exemption is content-based rather
/// than symlink-identity: after a switch, Claude Code rewrites
/// `~/.claude/.credentials.json` as a REGULAR-FILE mirror of the Keychain,
/// clobbering clauth's symlink with identical content. Capturing a `setup-token`
/// sidecar for the ACTIVE profile then flips the install source to
/// `session-token.json`, so classify reads Diverged over that regular file —
/// yet the live OAuth login is fully saved in the profile's `credentials.json`.
/// `live_login_is_stored` must exempt it by CONTENT (a symlink-identity check
/// reads a regular file as unsaved and defers every switch). Runs on every
/// platform — the content path is what makes the fix portable — so a Linux CI
/// exercises the macOS shape the maintainer can't.
#[test]
fn a_regular_file_mirror_of_a_stored_login_is_not_unsaved() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    // CC's regular-file mirror: same OAuth login as the stored credentials.json,
    // written as a plain file (not our symlink).
    let live = claude_credentials_path().expect("creds path");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    fs::write(
        &live,
        serde_json::to_vec(&creds("usage-access", Some("usage-refresh"))).expect("ser"),
    )
    .expect("write regular-file mirror");

    // The sidecar capture flips the install source; classify reads Diverged over
    // the regular file (it no longer matches what a switch installs).
    fill_session_token_by_hand("split", "oat-access");
    assert!(
        matches!(
            classify_credentials_link("split").expect("classify"),
            LinkState::Diverged
        ),
        "the mirror no longer matches the flipped install source"
    );
    assert!(
        live_login_is_stored("split"),
        "…but the mirror's login is saved in credentials.json — not unsaved \
         (a symlink-identity check would read this regular file as unsaved)"
    );

    // A genuine CC re-login (a DIFFERENT token) is the state the gates exist for —
    // it matches neither store, so it is protected.
    fs::write(
        &live,
        serde_json::to_vec(&creds("cc-relogin", Some("cc-rt"))).expect("ser"),
    )
    .expect("write regular re-login");
    assert!(
        !live_login_is_stored("split"),
        "a re-login whose token matches no store must stay protected"
    );

    // Absent live slot: nothing to match, nothing saved.
    fs::remove_file(&live).expect("drop file");
    assert!(!live_login_is_stored("split"));
}

/// The symlink half of the same exemption, and the original 2026-07-21 repro:
/// capturing a sidecar for the ACTIVE profile flips the install source while the
/// live slot is still clauth's symlink into `credentials.json`. classify reads
/// Diverged (the link no longer points at what a switch installs), but a
/// clauth-owned symlink's target IS a profile store by construction, so nothing
/// is unsaved — `live_login_is_stored` exempts it both structurally (it's a
/// symlink) and by content (reading through it yields the stored login).
#[cfg(unix)]
#[test]
fn a_clauth_symlink_under_a_flipped_install_source_is_not_unsaved() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let live = claude_credentials_path().expect("creds path");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    let store = crate::profile::profile_dir("split")
        .expect("dir")
        .join("credentials.json");
    std::os::unix::fs::symlink(&store, &live).expect("symlink live");
    assert!(
        matches!(
            classify_credentials_link("split").expect("classify"),
            LinkState::LinkedTo
        ),
        "before the capture the link points at the install source"
    );

    fill_session_token_by_hand("split", "oat-access");
    assert!(
        matches!(
            classify_credentials_link("split").expect("classify"),
            LinkState::Diverged
        ),
        "the stale link no longer points at what a switch installs"
    );
    assert!(
        live_login_is_stored("split"),
        "…but a clauth-owned symlink holds nothing unsaved"
    );

    // A dangling clauth symlink (its store file removed) still has no login to
    // protect — the structural half keeps exempting it, so a switch is never
    // deferred over an empty slot.
    fs::remove_file(&store).expect("drop store file");
    assert!(
        live_login_is_stored("split"),
        "a dangling clauth symlink is a store slot, not an unsaved login"
    );
}

// ---------------------------------------------------------------------------
// mcpOAuth preservation. `~/.claude/.credentials.json` also holds each MCP
// server's OAuth login (`mcpOAuth`), which is independent of the Claude account;
// an account switch must not drop them. Every token below is synthetic.
// ---------------------------------------------------------------------------

/// A synthetic live-credentials body: a Claude login plus one MCP-server login.
fn live_with_mcp(login: &str, mcp_token: &str) -> serde_json::Value {
    serde_json::json!({
        "claudeAiOauth": { "accessToken": login },
        "mcpOAuth": { "linear": { "accessToken": mcp_token } }
    })
}

#[test]
fn carry_copies_mcp_oauth_and_leaves_the_target_login_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = dir.path().join("live.json");
    let target = dir.path().join("credentials.json");
    fs::write(
        &live,
        serde_json::to_vec(&live_with_mcp("live-login", "mock-linear")).unwrap(),
    )
    .expect("write live");
    // Target store is a fresh browser login: a Claude login, no mcpOAuth.
    fs::write(
        &target,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "target-login" } }),
        )
        .unwrap(),
    )
    .expect("write target");

    carry_live_extra_into(&live, &target).expect("carry");

    let got: serde_json::Value =
        serde_json::from_slice(&fs::read(&target).expect("read target")).expect("parse");
    assert_eq!(
        got["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "mcpOAuth carried onto the incoming profile"
    );
    assert_eq!(
        got["claudeAiOauth"]["accessToken"], "target-login",
        "the incoming account's own login is never overwritten by the live one"
    );
}

/// The accepted ceiling, pinned end-to-end rather than at the helper: the carry
/// can add and overwrite, never delete, so a block the live file lacks survives
/// onto the live slot when that store becomes live. Pruning instead would wipe
/// real logins the first time a freshly-logged-in account went live. Anyone who
/// adds pruning fails here and has to go read why it is deliberate.
#[cfg(unix)]
#[test]
fn a_block_the_live_file_lacks_survives_onto_the_live_slot() {
    let _home = HomeSandbox::new();
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("login-b", Some("refresh-b")));
    crate::profile::save_profile(&b).expect("save b");

    // B's store already holds an MCP login from an earlier era; the live file
    // (A, freshly logged in through the browser) carries none.
    let b_store = crate::profile::profile_dir("b")
        .expect("dir")
        .join("credentials.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&b_store).expect("read b")).expect("parse");
    stored["mcpOAuth"] = serde_json::json!({ "sentry": { "accessToken": "mock-sentry" } });
    fs::write(&b_store, serde_json::to_vec(&stored).unwrap()).expect("seed b");
    force_link_profile_credentials("a").expect("link a");

    force_link_profile_credentials("b").expect("link b");

    let live_path = claude_credentials_path().expect("creds path");
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read after")).expect("parse");
    assert_eq!(
        after["mcpOAuth"]["sentry"]["accessToken"], "mock-sentry",
        "a login-only live file must not prune the incoming store's own blocks"
    );
}

/// The static-token sidecar is built by [`write_session_token`] from the mint
/// alone, so anything carried into it is dropped at the next re-mint. Driven
/// through the production writer on purpose: the sidecar DOES carry a
/// `claudeAiOauth` block, so a content-shaped guard reads it as an OAuth store
/// and writes MCP secrets into it.
#[test]
fn carry_skips_the_static_token_sidecar() {
    let _home = HomeSandbox::new();
    let mut split = crate::profile::Profile::new("split".to_string(), None, None);
    split.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&split).expect("save split");
    let target = crate::profile::profile_dir("split")
        .expect("dir")
        .join("session-token.json");
    crate::claude::write_session_token("split", &format!("sk-ant-{}", "m".repeat(40)), 0)
        .expect("mint");
    let before = fs::read(&target).expect("read sidecar");

    let live = crate::profile::profile_dir("split")
        .expect("dir")
        .join("live.json");
    fs::write(
        &live,
        serde_json::to_vec(&live_with_mcp("live-login", "mock-linear")).unwrap(),
    )
    .expect("write live");

    carry_live_extra_into(&live, &target).expect("carry");

    assert_eq!(
        fs::read(&target).expect("read sidecar"),
        before,
        "the sidecar is rebuilt from the mint on every re-mint, so nothing may be carried into it"
    );
}

/// The carry is an allowlist: only `mcpOAuth` moves. Any other non-login key
/// Claude Code parks in that store stays with the account that minted it.
#[test]
fn carry_moves_only_the_allowlisted_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = dir.path().join("live.json");
    let target = dir.path().join("credentials.json");
    fs::write(
        &live,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "live-login" },
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } },
            "trustedDeviceToken": "mock-device-token"
        }))
        .unwrap(),
    )
    .expect("write live");
    fs::write(
        &target,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "target-login" } }),
        )
        .unwrap(),
    )
    .expect("write target");

    carry_live_extra_into(&live, &target).expect("carry");

    let got: serde_json::Value =
        serde_json::from_slice(&fs::read(&target).expect("read target")).expect("parse");
    assert_eq!(
        got["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the allowlisted key is carried"
    );
    assert!(
        got.get("trustedDeviceToken").is_none(),
        "an unrecognised key must not cross accounts on a switch"
    );
}

/// The carry is a new writer of a file under `~/.clauth`, so it owes the tree's
/// 0600 invariant like every other one.
#[cfg(unix)]
#[test]
fn carry_keeps_the_store_at_0600() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let target = crate::profile::profile_dir("a")
        .expect("dir")
        .join("credentials.json");

    let live = crate::profile::profile_dir("a")
        .expect("dir")
        .join("live.json");
    fs::write(
        &live,
        serde_json::to_vec(&live_with_mcp("live-login", "mock-linear")).unwrap(),
    )
    .expect("write live");

    carry_live_extra_into(&live, &target).expect("carry");

    // Assert the write HAPPENED before asserting its mode: a carry that never
    // runs leaves the mode `save_profile` set, so a mode check alone passes
    // against a no-op and the posture it claims to pin goes uncovered.
    let got: serde_json::Value =
        serde_json::from_slice(&fs::read(&target).expect("read target")).expect("parse");
    assert_eq!(
        got["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the carry must have rewritten the store for its mode to mean anything"
    );
    let mode = fs::metadata(&target).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the carry must not widen the store's mode");
}

#[cfg(unix)]
#[test]
fn switching_accounts_preserves_mcp_oauth_end_to_end() {
    let _home = HomeSandbox::new();

    // Two OAuth profiles, each login-only in its store (as a browser login lands).
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("login-b", Some("refresh-b")));
    crate::profile::save_profile(&b).expect("save b");

    // Make A live, then simulate Claude Code authenticating an MCP server: it
    // writes an mcpOAuth block through clauth's symlink into A's store.
    force_link_profile_credentials("a").expect("link a");
    let live_path = claude_credentials_path().expect("creds path");
    let mut live: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read live")).expect("parse live");
    live["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    fs::write(&live_path, serde_json::to_vec(&live).unwrap()).expect("write live mcp");

    // Switch to B.
    force_link_profile_credentials("b").expect("link b");

    // The live credential now resolves to B's login AND still carries mcpOAuth.
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read after")).expect("parse after");
    assert_eq!(
        after["claudeAiOauth"]["accessToken"], "login-b",
        "the switch installed account B's login"
    );
    assert_eq!(
        after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the MCP-server login survived the account switch"
    );
}

#[test]
fn link_adopts_a_matching_login_and_preserves_mcp_oauth() {
    let _home = HomeSandbox::new();
    // Profile "main" holds a login-only store — exactly how a snapshot of the live
    // account records it, since the typed model drops mcpOAuth.
    let mut main = crate::profile::Profile::new("main".to_string(), None, None);
    main.credentials = Some(creds("acct-login", Some("acct-refresh")));
    crate::profile::save_profile(&main).expect("save main");

    // The live file is the SAME account (an untracked regular file) carrying an
    // mcpOAuth block — the state that made the byte-compare guard falsely refuse.
    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "acct-login" },
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
        }))
        .unwrap(),
    )
    .expect("write live");

    // Must NOT refuse (same login), and must carry mcpOAuth onto the store.
    link_profile_credentials("main").expect("link adopts a matching login");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read after")).expect("parse");
    assert_eq!(after["claudeAiOauth"]["accessToken"], "acct-login");
    assert_eq!(
        after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "mcpOAuth survived the adoption link"
    );
}

/// The refuse-guard reads "the live path is not a symlink" as "something other
/// than clauth wrote it". That premise holds only where clauth's own link IS a
/// symlink. On a host whose transport falls back to a copy — Windows without
/// `SeCreateSymbolicLinkPrivilege`, where `create_symlink` copies — clauth writes
/// a plain file itself, so the guard fires on clauth's OWN artifact and refuses
/// with a divergence the TUI it points at cannot resolve.
///
/// Both arms off ONE fixture: the guard is scoped to the transport whose premise
/// it rests on, not deleted. The refusing arm runs first, since it bails without
/// touching the live file.
#[test]
fn a_copy_transport_host_relinks_over_the_plain_file_it_wrote_itself() {
    let _home = HomeSandbox::new();
    let mut acme = crate::profile::Profile::new("acme".to_string(), None, None);
    acme.credentials = Some(creds("acme-login", Some("acme-refresh")));
    crate::profile::save_profile(&acme).expect("save acme");

    // What a copy transport leaves once the store is rewritten under it: a plain
    // file holding the PREVIOUS login, so live and stored genuinely differ.
    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "acme-login-before" } }),
        )
        .unwrap(),
    )
    .expect("write live");

    let err = link_profile_credentials("acme")
        .expect_err("a real-symlink host must still refuse an unresolved live file");
    assert!(
        err.to_string().contains("refusing to replace"),
        "the guard stays live where a plain file means somebody else wrote it: {err}"
    );

    crate::runtime::with_link_mode(crate::runtime::LinkMode::Fake, || {
        link_profile_credentials("acme")
    })
    .expect("a copy-transport host must relink over the plain file it wrote itself");
    assert_eq!(
        crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&live_path)
            .expect("read relinked live")
            .access_token(),
        Some("acme-login"),
        "the relink must land the profile's stored login, not leave the stale one"
    );
}

#[test]
fn link_still_refuses_a_different_live_login() {
    let _home = HomeSandbox::new();
    let mut other = crate::profile::Profile::new("other".to_string(), None, None);
    other.credentials = Some(creds("other-login", Some("other-refresh")));
    crate::profile::save_profile(&other).expect("save other");

    // Live is an unresolved DIFFERENT account — a CC re-login the user hasn't saved.
    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "unsaved-live-login" } }),
        )
        .unwrap(),
    )
    .expect("write live");

    let err = link_profile_credentials("other").expect_err("must refuse a different login");
    assert!(
        err.to_string().contains("refusing to replace"),
        "the guard still protects an unresolved different login: {err}"
    );
}

/// The refuse-guard compares logins, so it must not read "neither side names a
/// login" as "the logins match". A live file too torn to parse yields no login,
/// and so does an install source that does not exist — which is every profile
/// storing no `credentials.json`. Left to the login test alone the two compare
/// equal, the guard clears, and the live file is removed with nothing to relink.
/// The byte-compare fallback is what keeps refusing here.
#[test]
fn link_refuses_a_torn_live_file_over_a_profile_storing_no_login() {
    let _home = HomeSandbox::new();
    // An api-key profile: saved, and storing no credentials.json at all.
    let mut endpoint = crate::profile::Profile::new(
        "endpoint".to_string(),
        Some("https://api.example.invalid".to_string()),
        Some("mock-key".to_string()),
    );
    endpoint.credentials = None;
    crate::profile::save_profile(&endpoint).expect("save endpoint");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    // Caught mid-write by CC: valid prefix, no closing brace.
    std::fs::write(&live_path, br#"{"claudeAiOauth":{"accessToken":"live"#).expect("write torn");

    let err = link_profile_credentials("endpoint").expect_err("must refuse a torn live file");
    assert!(
        err.to_string().contains("refusing to replace"),
        "a file too torn to parse is a possible mid-write login, not a match: {err}"
    );
    assert!(
        live_path.exists(),
        "the torn live file must survive the refusal, not be deleted with nothing to relink"
    );
}

/// Same hole, reached by the other route: a live file carrying MCP-server logins
/// and no Claude login block parses fine and still names no login.
#[test]
fn link_refuses_a_login_less_live_file_over_a_profile_storing_no_login() {
    let _home = HomeSandbox::new();
    let mut endpoint = crate::profile::Profile::new(
        "endpoint".to_string(),
        Some("https://api.example.invalid".to_string()),
        Some("mock-key".to_string()),
    );
    endpoint.credentials = None;
    crate::profile::save_profile(&endpoint).expect("save endpoint");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(&serde_json::json!({
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
        }))
        .unwrap(),
    )
    .expect("write login-less live");

    let err = link_profile_credentials("endpoint").expect_err("must refuse a login-less live file");
    assert!(
        err.to_string().contains("refusing to replace"),
        "no login on either side is not a matching login: {err}"
    );
    assert!(
        live_path.exists(),
        "the MCP blocks must survive the refusal — deleting them is the loss this feature exists to stop"
    );
}

/// Two logged-out shells are two blank tokens, and blank equals blank. The login
/// test carries `classify_link_at`'s non-empty clause so it never clears on them;
/// differing shells then fall to the byte compare and refuse.
#[test]
fn link_refuses_two_differing_logged_out_shells() {
    let _home = HomeSandbox::new();
    let mut acct = crate::profile::Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(creds("", Some("")));
    crate::profile::save_profile(&acct).expect("save acct");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "", "refreshToken": "", "expiresAt": 0 },
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
        }))
        .unwrap(),
    )
    .expect("write shell");

    let err = link_profile_credentials("acct").expect_err("must refuse two blank logins");
    assert!(
        err.to_string().contains("refusing to replace"),
        "a blank token must never match another blank token: {err}"
    );
}

/// A carry failure must not strand the operator on the outgoing account.
/// Preserving MCP logins is a convenience; completing the switch is not, so an
/// unwritable profile directory reports and continues.
#[cfg(unix)]
#[test]
fn a_failed_carry_still_completes_the_switch() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("login-b", Some("refresh-b")));
    crate::profile::save_profile(&b).expect("save b");

    force_link_profile_credentials("a").expect("link a");
    let live_path = claude_credentials_path().expect("creds path");
    let mut live: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read live")).expect("parse live");
    live["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    fs::write(&live_path, serde_json::to_vec(&live).unwrap()).expect("write live mcp");

    // Lock B's directory so the carry's atomic write cannot land its temp file.
    let b_dir = crate::profile::profile_dir("b").expect("dir");
    fs::set_permissions(&b_dir, fs::Permissions::from_mode(0o500)).expect("lock b");
    if fs::write(b_dir.join(".probe"), b"x").is_ok() {
        // Running as root: mode bits do not deny, so there is no failure to drive.
        fs::set_permissions(&b_dir, fs::Permissions::from_mode(0o700)).expect("unlock b");
        return;
    }

    let result = force_link_profile_credentials("b");
    fs::set_permissions(&b_dir, fs::Permissions::from_mode(0o700)).expect("unlock b");
    result.expect("an unwritable store must not fail the switch");

    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read after")).expect("parse after");
    assert_eq!(
        after["claudeAiOauth"]["accessToken"], "login-b",
        "the switch installed account B even though its MCP carry could not land"
    );
}
