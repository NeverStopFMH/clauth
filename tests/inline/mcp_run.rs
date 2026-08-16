#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! `delegate` recursion-guard coverage. With `CLAUTH_MCP_DEPTH >= 1` the delegate must
//! short-circuit to an `is_error` envelope BEFORE any `claude` spawn (the
//! fork-bomb cap). We assert the error envelope without faking a `claude` binary;
//! the guard returns before `spawn_blocking`/`ProfileRuntime::acquire` runs.

use super::*;
use crate::testutil::HomeSandbox;

/// Drive the async `delegate` tool with `CLAUTH_MCP_DEPTH = depth` on a current-thread
/// runtime, restoring the prior env value before returning.
///
/// # Safety
/// `set_var`/`remove_var` are unsafe in Rust 2024 (not thread-safe). The lock
/// only serializes tests that also take it (the env/FS tests, now including
/// `update.rs`'s `with_no_update_env`); a test mutating env without it could
/// still race. Restored before the function returns, so no other thread that
/// holds the lock observes a torn value.
fn run_with_depth(depth: &str) -> CallToolResult {
    run_with_depth_args(
        depth,
        DelegateArgs {
            profiles: Some(vec!["any".to_string()]),
            prompt: Some("hello".to_string()),
            prompt_file: None,
            model: None,
            cwd: None,
            env: None,
            args: None,
            timeout_secs: None,
            idle_secs: None,
            resume: None,
            isolated: None,
            background: None,
        },
    )
}

fn run_with_depth_args(depth: &str, args: DelegateArgs) -> CallToolResult {
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the lock above, restored unconditionally.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, depth) };

    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let result = rt.block_on(async { server.delegate(Parameters(args)).await });

    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }
    result.expect("delegate returns a tool result, never a transport error")
}

#[test]
fn depth_guard_refuses_at_depth_one_without_spawning() {
    let result = run_with_depth("1");

    assert_eq!(
        result.is_error,
        Some(true),
        "delegate at depth 1 is a tool error"
    );

    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("error envelope text");
    // The refusal fires before target validation, so it names the caller's
    // own spelling — `profiles` now, there is no singular field.
    assert!(text.contains("delegate to `any` failed: delegation depth exceeded (max 1)"));
}

#[test]
fn depth_guard_also_refuses_above_one() {
    let result = run_with_depth("3");
    assert_eq!(result.is_error, Some(true));
}

#[test]
fn depth_guard_names_the_fanout_targets_the_caller_spelled() {
    let fanout_args = || DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hello".to_string()),
        prompt_file: None,
        model: None,
        cwd: None,
        env: None,
        args: None,
        timeout_secs: None,
        idle_secs: None,
        resume: None,
        isolated: None,
        background: Some(true),
    };

    // The reply names the caller's targets end to end, never `unknown`.
    let prose = run_with_depth_args("1", fanout_args());
    assert_eq!(prose.is_error, Some(true), "the refusal is a tool error");
    let text = prose
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("prose block");
    assert_eq!(
        text, "delegate to `solo`, `vendor` failed: delegation depth exceeded (max 1)",
        "prose names the targets the caller spelled"
    );
}

/// Mirrors `disable_profile`'s own live-session refusal from the other
/// direction: that guard stops disabling a profile mid-session, this one
/// stops `delegate` from opening a brand-new session on one already
/// disabled. Drives `run_delegate` directly — no async tool call, no `claude`
/// binary needed, since the guard fires before `ProfileRuntime::acquire`.
#[test]
fn run_delegate_refuses_a_disabled_target_before_acquiring_a_runtime() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "off".to_string(), None, None, None)
        .expect("create profile");
    crate::actions::disable_profile(&mut config, "off").expect("disable profile");

    let err = run_delegate(DelegateOpts {
        profile: "off",
        prompt: "hello",
        model: None,
        cwd: None,
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("a disabled target must be refused");
    assert_eq!(err, "profile is disabled: off (run `clauth enable off`)");

    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("off")
            .join("runtime")
            .exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

/// The quarantine gate `switch` has always had, moved onto the spend path. A
/// profile whose refresh token was rejected (AUTH-1) authenticates nothing, and
/// clauth knows it from `AppState::auth_broken` without touching the network —
/// so the refusal is the SAME sentence `switch` refuses with, not a
/// `claude exited with 1` after the window is already gone.
///
/// The nonexistent `cwd` is the fixture's own control: without the gate this
/// call runs on to the cwd check and fails there instead, which is what the
/// red looked like.
#[test]
fn run_delegate_refuses_an_auth_broken_target_before_acquiring_a_runtime() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "quarantined".to_string(), None, None, None)
        .expect("create profile");
    assert!(
        config.set_auth_broken("quarantined", true),
        "fixture control: the profile was not already quarantined",
    );
    crate::profile::save_app_state(&config.state).expect("persist the quarantine");

    let bad_cwd = home.home().join("does-not-exist");
    let err = run_delegate(DelegateOpts {
        profile: "quarantined",
        prompt: "hello",
        model: None,
        cwd: bad_cwd.to_str(),
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("a quarantined target must be refused");
    assert_eq!(err, crate::format::login_expired("quarantined").line());

    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("quarantined")
            .join("runtime")
            .exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

// ── third-party auth guard ──────────────────────────────────────────────────

/// Mirrors the disabled-target test above: a recognised third-party profile
/// with nothing to authenticate inference (no usable api key, no auth env
/// entry) is refused before `ProfileRuntime::acquire`, because the spawned
/// `claude` dies on an empty envelope. The keyed / OAuth / env-authed tests
/// below are the canaries that the guard does not over-fire.
#[test]
fn run_delegate_refuses_a_keyless_third_party_target_before_acquiring_a_runtime() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-nokey".to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");

    let err = run_delegate(DelegateOpts {
        profile: "ds-nokey",
        prompt: "hello",
        model: None,
        cwd: None,
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("a keyless third-party target must be refused");
    assert_eq!(
        err,
        "profile has no api key: ds-nokey (run `clauth login ds-nokey --api-key <key>`)"
    );

    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("ds-nokey")
            .join("runtime")
            .exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

/// An EMPTY api key is no credential: a `config.toml` with `api_key = ""` used
/// to pass the guard's old `is_some()` test and spawned a `claude` that
/// authenticates with an empty token and dies. Same refusal, same
/// before-acquire guarantee, as the keyless test above.
#[test]
fn run_delegate_refuses_an_empty_api_key_third_party_target() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-empty".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some(String::new()),
        None,
    )
    .expect("create profile");

    let err = run_delegate(DelegateOpts {
        profile: "ds-empty",
        prompt: "hello",
        model: None,
        cwd: None,
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("an empty-key third-party target must be refused");
    assert_eq!(
        err,
        "profile has no api key: ds-empty (run `clauth login ds-empty --api-key <key>`)"
    );

    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("ds-empty")
            .join("runtime")
            .exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

/// The whitespace sibling: a key of only spaces authenticates nothing either,
/// and the load boundary's `has_usable_key` already trims before testing, so
/// the guard must read it as absent too.
#[test]
fn run_delegate_refuses_a_whitespace_api_key_third_party_target() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-space".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("   ".to_string()),
        None,
    )
    .expect("create profile");

    let err = run_delegate(DelegateOpts {
        profile: "ds-space",
        prompt: "hello",
        model: None,
        cwd: None,
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("a whitespace-key third-party target must be refused");
    assert_eq!(
        err,
        "profile has no api key: ds-space (run `clauth login ds-space --api-key <key>`)"
    );

    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("ds-space")
            .join("runtime")
            .exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

/// An interior control char survives `trim` + non-empty but fails
/// `validate_api_key` (CC forwards the minted key verbatim as `X-Api-Key` /
/// `Authorization: Bearer`, so a CRLF would inject a header). Only a guard
/// predicate that includes the validation refuses it.
#[test]
fn run_delegate_refuses_a_control_char_api_key_third_party_target() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-ctrl".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test\r\nInjected: x".to_string()),
        None,
    )
    .expect("create profile");

    let err = run_delegate(DelegateOpts {
        profile: "ds-ctrl",
        prompt: "hello",
        model: None,
        cwd: None,
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("a control-char key third-party target must be refused");
    assert_eq!(
        err,
        "profile has no api key: ds-ctrl (run `clauth login ds-ctrl --api-key <key>`)"
    );

    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("ds-ctrl")
            .join("runtime")
            .exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

/// Drive `run_delegate` for `profile` with a `cwd` that does not exist. The
/// cwd check is the first gate AFTER the credential guard, so reaching it
/// proves the guard let the profile through while the run still stops before
/// `ProfileRuntime::acquire` or any spawn. Returns the refusal reason.
fn delegate_stops_at_the_cwd_gate(profile: &str, cwd: &std::path::Path) -> String {
    run_delegate(DelegateOpts {
        profile,
        prompt: "hello",
        model: None,
        cwd: cwd.to_str(),
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("the cwd gate must refuse the run")
}

#[test]
fn run_delegate_does_not_refuse_a_keyed_third_party_target() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-key".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test".to_string()),
        None,
    )
    .expect("create profile");

    let bad_cwd = home.home().join("does-not-exist");
    let err = delegate_stops_at_the_cwd_gate("ds-key", &bad_cwd);
    assert_eq!(
        err,
        format!(
            "cwd does not exist or is not a directory: {}",
            bad_cwd.display()
        ),
        "a keyed third-party profile passes the guard and stops at the cwd gate"
    );
}

#[test]
fn run_delegate_does_not_refuse_an_oauth_profile() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "oauth1".to_string(), None, None, None)
        .expect("create profile");

    let bad_cwd = home.home().join("does-not-exist");
    let err = delegate_stops_at_the_cwd_gate("oauth1", &bad_cwd);
    assert_eq!(
        err,
        format!(
            "cwd does not exist or is not a directory: {}",
            bad_cwd.display()
        ),
        "an OAuth profile passes the guard and stops at the cwd gate"
    );
}

/// The console session authenticates the QUOTA gateway only
/// (`docs/providers.md`): inference on a keyless Alibaba profile needs the api
/// key like every other provider. The guard refuses it rather than spending a
/// window on a run that cannot authenticate.
#[test]
fn run_delegate_refuses_a_keyless_alibaba_profile() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "qwen".to_string(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com".to_string()),
        None,
        None,
    )
    .expect("create profile");

    let err = run_delegate(DelegateOpts {
        profile: "qwen",
        prompt: "hello",
        model: None,
        cwd: None,
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Shared,
        depth: 0,
        job: None,
        cancel: None,
    })
    .expect_err("a keyless Alibaba profile must be refused");
    assert_eq!(
        err,
        "profile has no api key: qwen (run `clauth login qwen --api-key <key>`)"
    );

    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("qwen")
            .join("runtime")
            .exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

/// An `[env]`-authenticated third-party profile passes the guard: with no
/// `api_key` field, `build_claude_settings_json` applies `profile.env` LAST, so
/// an explicit `ANTHROPIC_AUTH_TOKEN` entry is what the spawned `claude`
/// authenticates with. Refusing this profile was the `7de66d7` regression.
#[test]
fn run_delegate_does_not_refuse_an_env_authenticated_third_party_profile() {
    let home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-env".to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");
    let profile = config.find_mut("ds-env").expect("profile created");
    profile.env.insert(
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        "sk-env-test".to_string(),
    );
    crate::profile::save_profile(profile).expect("persist the env entry");

    let bad_cwd = home.home().join("does-not-exist");
    let err = delegate_stops_at_the_cwd_gate("ds-env", &bad_cwd);
    assert_eq!(
        err,
        format!(
            "cwd does not exist or is not a directory: {}",
            bad_cwd.display()
        ),
        "an env-authenticated profile passes the guard and stops at the cwd gate"
    );
}

#[test]
fn resolve_fanout_refuses_a_keyless_third_party_member_by_name() {
    let _home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-key".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test".to_string()),
        None,
    )
    .expect("create profile");
    crate::actions::create_blank_profile(&mut config, "oauth1".to_string(), None, None, None)
        .expect("create profile");
    crate::actions::create_blank_profile(
        &mut config,
        "qwen".to_string(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com".to_string()),
        None,
        None,
    )
    .expect("create profile");
    crate::actions::create_blank_profile(
        &mut config,
        "ds-nokey".to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");

    // The keyed and OAuth members come FIRST on purpose: if the guard
    // over-fired on either the refusal would name that name and this
    // assertion would red. `qwen` is the first refuser now — its console
    // session does not authenticate inference — and `ds-nokey` sits behind it.
    let raw: Vec<String> = ["ds-key", "oauth1", "qwen", "ds-nokey"]
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    let err =
        resolve_fanout(&config, &raw).expect_err("a keyless member refuses the whole fan-out");
    assert_eq!(
        err,
        "profile has no api key: qwen (run `clauth login qwen --api-key <key>`)"
    );
}

/// The fan-out sibling of the empty-key single-profile test: an empty (or
/// whitespace-only) api key on any member refuses the whole list by name,
/// before the first spawn.
#[test]
fn resolve_fanout_refuses_an_empty_key_member_by_name() {
    let _home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-key".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test".to_string()),
        None,
    )
    .expect("create profile");
    crate::actions::create_blank_profile(&mut config, "oauth1".to_string(), None, None, None)
        .expect("create profile");
    crate::actions::create_blank_profile(
        &mut config,
        "ds-empty".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some(String::new()),
        None,
    )
    .expect("create profile");
    crate::actions::create_blank_profile(
        &mut config,
        "ds-space".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some(" \t ".to_string()),
        None,
    )
    .expect("create profile");

    // The keyed and OAuth members come FIRST on purpose: if the guard
    // over-fired on either the refusal would name that name and this
    // assertion would red.
    let raw: Vec<String> = ["ds-key", "oauth1", "ds-empty", "ds-space"]
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    let err =
        resolve_fanout(&config, &raw).expect_err("an empty-key member refuses the whole fan-out");
    assert_eq!(
        err,
        "profile has no api key: ds-empty (run `clauth login ds-empty --api-key <key>`)"
    );
}

/// The quarantine sibling: a fan-out member whose refresh token was rejected
/// refuses the whole list before the first spawn, in `switch`'s own words. The
/// quarantine is a pure in-memory read of `AppState::auth_broken`, so nothing
/// here touches the network — an MCP-side refresh would invert the lock order.
#[test]
fn resolve_fanout_refuses_an_auth_broken_member_by_name() {
    let _home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-key".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test".to_string()),
        None,
    )
    .expect("create profile");
    crate::actions::create_blank_profile(&mut config, "oauth1".to_string(), None, None, None)
        .expect("create profile");
    crate::actions::create_blank_profile(&mut config, "dead".to_string(), None, None, None)
        .expect("create profile");
    assert!(
        config.set_auth_broken("dead", true),
        "fixture control: the member was not already quarantined",
    );

    // The healthy members come FIRST on purpose: an over-firing gate would name
    // one of them and this assertion would red.
    let raw: Vec<String> = ["ds-key", "oauth1", "dead"]
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    let err =
        resolve_fanout(&config, &raw).expect_err("a quarantined member refuses the whole fan-out");
    assert_eq!(err, crate::format::login_expired("dead").line());
}

/// Gate ORDER, pinned because nothing else can hold it: a target that is both
/// disabled and quarantined refuses as DISABLED. `switch` orders the two the
/// same way, so a disabled target is bailed on before anything can reach its
/// single-use refresh token, and the operator is not told to re-login an
/// account they deliberately turned off.
#[test]
fn a_disabled_and_quarantined_target_refuses_as_disabled() {
    let _home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "both".to_string(), None, None, None)
        .expect("create profile");
    crate::actions::disable_profile(&mut config, "both").expect("disable profile");
    assert!(
        config.set_auth_broken("both", true),
        "fixture control: the profile was not already quarantined",
    );

    let raw = vec!["both".to_string()];
    let err = resolve_fanout(&config, &raw).expect_err("a disabled member refuses the fan-out");
    assert_eq!(err, "profile is disabled: both (run `clauth enable both`)");
}

#[test]
fn resolve_fanout_passes_when_every_member_is_delegable() {
    let _home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ds-key".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test".to_string()),
        None,
    )
    .expect("create profile");
    crate::actions::create_blank_profile(&mut config, "oauth1".to_string(), None, None, None)
        .expect("create profile");
    // A keyless Alibaba member would now refuse the fan-out; a keyed one is
    // delegable like any other provider.
    crate::actions::create_blank_profile(
        &mut config,
        "qwen".to_string(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com".to_string()),
        Some("sk-test".to_string()),
        None,
    )
    .expect("create profile");

    let raw: Vec<String> = ["ds-key", "oauth1", "qwen"]
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    let names = resolve_fanout(&config, &raw).expect("every member has a credential");
    assert_eq!(names, vec!["ds-key", "oauth1", "qwen"]);
}

/// The handler-level pin: a background fan-out with a keyless member refuses
/// before any job file is reserved, so no account gets spent under a call
/// reported as failed. Mirrors `background_depth_guard_refuses_without_writing_job`.
#[test]
fn background_fanout_refuses_a_keyless_member_before_writing_jobs() {
    let _home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "solo".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test".to_string()),
        None,
    )
    .expect("create profile");
    crate::actions::create_blank_profile(
        &mut config,
        "vendor".to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");

    // Pin the depth to 0: the host that runs this suite may itself be a
    // delegate child (`CLAUTH_MCP_DEPTH=1`), which would refuse at the depth
    // guard before the fan-out guard this test pins.
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by HOME_TEST_LOCK (held by the sandbox),
    // restored unconditionally below.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, "0") };

    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let result = rt
        .block_on(async {
            server
                .delegate(Parameters(DelegateArgs {
                    profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
                    prompt: Some("hello".to_string()),
                    prompt_file: None,
                    model: None,
                    cwd: None,
                    env: None,
                    args: None,
                    timeout_secs: None,
                    idle_secs: None,
                    resume: None,
                    isolated: None,
                    background: Some(true),
                }))
                .await
        })
        .expect("delegate returns a tool result, never a transport error");

    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }

    assert_eq!(
        result.is_error,
        Some(true),
        "a keyless member refuses the fan-out"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("refusal text");
    assert_eq!(
        text,
        "delegate failed: profile has no api key: vendor \
         (run `clauth login vendor --api-key <key>`)",
        "the refusal names the keyless profile, what it lacks, and the fix",
    );
    let job_count = jobs::jobs_dir()
        .ok()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    assert_eq!(job_count, 0, "a refused fan-out writes no job file");
}

// TODO(manual/integration): the live-spawn paths cannot be unit-tested without a
// real `claude` on PATH, and we deliberately do NOT fake one (a fake binary
// would assert nothing about the real envelope contract). Verify by hand:
//   1. concurrent-different-profile: `delegate` two different profiles at once; each
//      gets its own runtime + PID namespace and they complete without contention.
//   2. same-profile rotation safety: with an interactive session of profile P
//      live, `delegate` P; the delegate shares P's runtime + `RotationGuard` flock and
//      gets a fresh token chain only after the live watchdog reconciles.
//   3. happy path: a valid prompt returns `{is_error:false, result, ...}` parsed
//      from `claude -p --output-format stream-json --verbose
//      --include-partial-messages`, and the child inherits `CLAUTH_MCP_DEPTH=1`
//      + `--strict-mcp-config`.
//   4. idle kill + salvage: `delegate` a prompt that writes a few paragraphs and
//      then runs a `sleep` past `idle_secs: 30`; the envelope comes back
//      `timed_out:"idle"` carrying those paragraphs in `partial_result`.

// ---- delegate env composition (provider-routing isolation) ----

#[test]
fn delegate_env_strips_inherited_provider_routing() {
    let mut cmd = Command::new("claude");
    apply_delegate_env(
        &mut cmd,
        &HashMap::new(),
        &[],
        std::path::Path::new("/cfg"),
        0,
    );
    let envs = crate::testutil::env_overrides(&cmd);

    // every provider-routing key is queued for removal so a parent session's
    // endpoint/token can't cross-route the delegate to the wrong provider.
    for key in crate::runtime::MANAGED_ENV_KEYS {
        assert_eq!(
            envs.get(*key),
            Some(&None),
            "{key} must be stripped from the inherited env",
        );
    }
    // clauth's own keys are always set.
    assert_eq!(
        envs.get("CLAUDE_CONFIG_DIR"),
        Some(&Some("/cfg".to_string()))
    );
    assert_eq!(envs.get("CLAUTH_MCP_DEPTH"), Some(&Some("1".to_string())));
    assert_eq!(
        envs.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
        Some(&Some(DEFAULT_MAX_OUTPUT_TOKENS.to_string())),
    );
}

#[test]
fn delegate_env_strips_active_profile_custom_env() {
    // the active profile's custom env keys are scrubbed from the inherited
    // process env too, so a delegate aimed at profile B drops profile A's
    // custom `[env]`. Mirrors the settings.json channel (active_env_keys).
    let mut cmd = Command::new("claude");
    apply_delegate_env(
        &mut cmd,
        &HashMap::new(),
        &["FOO".to_string(), "BAR".to_string()],
        std::path::Path::new("/cfg"),
        0,
    );
    let envs = crate::testutil::env_overrides(&cmd);
    assert_eq!(
        envs.get("FOO"),
        Some(&None),
        "active custom env key must be stripped",
    );
    assert_eq!(envs.get("BAR"), Some(&None));
}

#[test]
fn delegate_env_caller_reauthority_and_clauth_keys_win() {
    let mut caller = HashMap::new();
    // a caller may deliberately re-route by re-adding a stripped key,
    caller.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://example.test".to_string(),
    );
    // must NOT be able to defeat the depth guard,
    caller.insert("CLAUTH_MCP_DEPTH".to_string(), "0".to_string());
    // and a caller-set max-tokens is respected, not overwritten by the default.
    caller.insert(
        "CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_string(),
        "999".to_string(),
    );

    let mut cmd = Command::new("claude");
    apply_delegate_env(&mut cmd, &caller, &[], std::path::Path::new("/cfg"), 0);
    let envs = crate::testutil::env_overrides(&cmd);

    assert_eq!(
        envs.get("ANTHROPIC_BASE_URL"),
        Some(&Some("https://example.test".to_string())),
        "a caller can re-add a stripped routing key deliberately",
    );
    assert_eq!(
        envs.get("CLAUTH_MCP_DEPTH"),
        Some(&Some("1".to_string())),
        "the depth guard always wins over a caller value",
    );
    assert_eq!(
        envs.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
        Some(&Some("999".to_string())),
        "a caller-set max-tokens is not clobbered by the default",
    );
}

// ---- background delegation + monitor ----

/// The reserved running record a test seeds a job from, carrying both deadlines
/// the way a real reserve does.
fn running_spec(job_id: &str, profile: &str, started_at: u64) -> jobs::RunningSpec {
    jobs::RunningSpec {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        started_at,
        timeout_secs: 3600,
        idle_secs: Some(300),
    }
}

/// Seed one `running` job file the way `reserve_background_job` would.
///
/// Every caller passes a real `now_ms()`, and that is not decoration: a `running`
/// record is reaped once it outlives the max delegate lifetime, so one stamped at
/// epoch 1 IS an orphaned corpse and the collect path is right to sweep it. A
/// `done` fixture has no such constraint — its retention runs from `done_at`,
/// and a reader never sweeps one — so those keep their arbitrary stamps.
fn seed_running(job_id: &str, profile: &str, started_at: u64) {
    jobs::write_running(&running_spec(job_id, profile, started_at)).unwrap();
}

/// Drive `monitor` with raw args on a current-thread runtime under a home
/// sandbox the caller has already entered. Enters through `monitor_with`, the
/// inner entry, because an in-process test cannot construct a
/// `Peer<RoleServer>`; `ProgressSink::none()` is also exactly what a peer that
/// sent no `progressToken` gets, so the path is a real one.
fn call_monitor_args(args: MonitorArgs) -> CallToolResult {
    let server = ClauthServer::new();
    // The wait loops sleep on tokio timers, which a bare current-thread runtime
    // does not arm.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(async { server.monitor_with(args, ProgressSink::none()).await })
        .expect("monitor returns a tool result, never a transport error")
}

/// Drive `monitor` on one job id (the single-job shape).
fn call_monitor(job_id: &str, wait_secs: Option<u64>) -> CallToolResult {
    call_monitor_args(MonitorArgs {
        job_ids: Some(vec![job_id.to_string()]),
        wait_secs,
        return_on: None,
        cancel: None,
    })
}

/// Drive `monitor` on several job ids (the batch shape).
fn call_monitor_batch(job_ids: Vec<&str>, wait_secs: Option<u64>) -> CallToolResult {
    call_monitor_args(MonitorArgs {
        job_ids: Some(job_ids.into_iter().map(str::to_string).collect()),
        wait_secs,
        return_on: None,
        cancel: None,
    })
}

#[test]
fn monitor_unknown_job_is_error() {
    let _home = HomeSandbox::new();
    let result = call_monitor("d-doesnotexist-0", Some(0));
    assert_eq!(
        result.is_error,
        Some(true),
        "unknown job_id is a tool error"
    );
}

#[test]
fn monitor_invalid_job_id_is_error() {
    let _home = HomeSandbox::new();
    let result = call_monitor("../escape", Some(0));
    assert_eq!(result.is_error, Some(true), "path-unsafe job_id refused");
}

/// The whole-second figure a running check rendered after `label`, e.g.
/// `"wall-kill in "` -> `2900`.
///
/// Every one of these is a floor-divided second computed from a stamp written at
/// one instant and read at another, so the rendered value is only ever pinned as
/// a RANGE: pinning the exact second makes the test hold on the gap between
/// those two instants staying under a millisecond boundary, which is a property
/// of the machine rather than of the code.
fn rendered_secs(text: &str, label: &str) -> u64 {
    let rest = text
        .split_once(label)
        .unwrap_or_else(|| panic!("{label:?} missing from: {text}"))
        .1;
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("no figure after {label:?} in: {text}"))
}

/// The prose of a running check, for the tests that read it.
fn monitor_text(job_id: &str) -> String {
    call_monitor(job_id, Some(0))
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("status text")
}

#[test]
fn monitor_running_reports_status() {
    let _home = HomeSandbox::new();
    seed_running("d-run-0", "work", now_ms());
    let result = call_monitor("d-run-0", Some(0));
    assert_ne!(result.is_error, Some(true), "a running job is not an error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("status text");
    assert!(
        text.starts_with("job `d-run-0` running on `work`, elapsed "),
        "running status with its id, account and elapsed time: {text}",
    );
    // The quota clause is unconditional now: the `monitor: true` flag that used
    // to gate it bought one free cache read and nothing else.
    assert!(
        text.contains("; quota: "),
        "a running check names the account's headroom: {text}",
    );
}

/// Finding 3/10: a running check's `quota` used to read `usage_cache.json` and
/// nothing else, so for the 17-of-29 accounts whose fetch leg writes the OTHER
/// cache the answer was `usage unknown` by construction — on the reply whose one
/// job is to say whether the account being spent still has headroom.
#[test]
fn a_running_check_on_a_third_party_target_reports_that_accounts_own_figures() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&crate::profile::Profile::new(
        "vendor".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save third-party profile");
    // Shaped-from-a-capture provider-cache bytes through the production reader:
    // the consumer must parse the shape the fetch leg actually writes.
    let cache = crate::profile_cache::profile_cache_path("vendor", THIRD_PARTY_CACHE_FILE).unwrap();
    std::fs::write(&cache, crate::testutil::THIRD_PARTY_CACHE_BYTES).expect("provider cache");
    seed_running("d-vendor-0", "vendor", now_ms());

    let text = monitor_text("d-vendor-0");
    assert!(
        text.contains("; quota: no Anthropic 5h/7d window; provider reports total: 31.45 CNY"),
        "the target's own cache answers, and the 5h/7d window it cannot have reads as none: {text}",
    );
    assert!(
        !text.contains("quota: usage unknown"),
        "clauth holds this account's figures, so nothing here is unknown: {text}",
    );
}

/// The half a selector keyed on `is_third_party` still answers wrong: a GENERIC
/// api-key endpoint has no typed integration, so `provider` is `None`, while the
/// same scheduler leg fetches and caches it (`ThirdPartyTarget::Generic`). Its
/// figures are on disk and every MCP surface must read them — the check's quota
/// and the folded live-usage clause both.
#[test]
fn a_generic_api_key_target_answers_with_the_figures_clauth_holds() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&crate::profile::Profile::new(
        "litellm".to_string(),
        Some("http://127.0.0.1:4000".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save generic api-key profile");
    let cache =
        crate::profile_cache::profile_cache_path("litellm", THIRD_PARTY_CACHE_FILE).unwrap();
    std::fs::write(&cache, crate::testutil::THIRD_PARTY_CACHE_BYTES).expect("provider cache");
    seed_running("d-generic-0", "litellm", now_ms());

    let text = monitor_text("d-generic-0");
    assert!(
        text.contains("; quota: no Anthropic 5h/7d window; provider reports total: 31.45 CNY"),
        "a generic endpoint's own cache answers its quota: {text}",
    );

    let folded = render::delegate_prose(&fold_delegate_live_usage(
        serde_json::json!({"is_error": false, "result": "ok"}),
        "litellm",
        0,
        DigestMode::Skip,
    ));
    assert!(
        folded.contains(
            "target `litellm`: no Anthropic 5h/7d window; provider reports total: 31.45 CNY"
        ),
        "and the same figures ride the delegate footer: {folded}",
    );
}

/// The cost basis asks where a request GOES, and an operator-authored
/// `[env] ANTHROPIC_BASE_URL` is where it goes: `build_claude_settings_json`
/// applies `profile.env` LAST, so such an entry wins over the managed
/// `base_url` field, and routes the run on its own when there is no managed
/// field at all. A predicate reading only the managed field prices that run at
/// Anthropic's card and lets the figure read as the bill.
///
/// A minimal pair driven through the REAL stored read: the two accounts differ
/// only by that env entry.
#[test]
fn an_env_authored_endpoint_qualifies_the_cost_clause() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&crate::profile::Profile::new(
        "plain".to_string(),
        None,
        None,
    ))
    .expect("save oauth profile");
    let mut env_only = crate::profile::Profile::new("envhost".to_string(), None, None);
    env_only.env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://api.deepseek.com/anthropic".to_string(),
    );
    crate::profile::save_profile(&env_only).expect("save env-endpoint profile");

    let priced = |name: &str| {
        render::delegate_prose(&fold_delegate_live_usage(
            serde_json::json!({"is_error": false, "result": "ok", "total_cost_usd": 2.06}),
            name,
            0,
            DigestMode::Skip,
        ))
    };
    let plain = priced("plain");
    assert!(
        plain.contains("(cost $2.06)"),
        "control: an account with no endpoint of any kind stays bare: {plain}",
    );
    let env_priced = priced("envhost");
    assert!(
        env_priced.contains("(cost $2.06 at Anthropic rates, not this endpoint's)"),
        "an env-only endpoint is still an endpoint: {env_priced}",
    );
}

/// The fail-safe arm, driven through the real read rather than a hand-built
/// payload: a name whose profile config cannot be read at all is `Unknown`,
/// never `Anthropic`. It earns the qualifier — and its OWN qualifier, because
/// `not this endpoint's` would assert an endpoint clauth never saw.
#[test]
fn an_unreadable_profile_config_prices_as_endpoint_unknown() {
    let _home = HomeSandbox::new();

    let prose = render::delegate_prose(&fold_delegate_live_usage(
        serde_json::json!({"is_error": false, "result": "ok", "total_cost_usd": 2.06}),
        "never-stored",
        0,
        DigestMode::Skip,
    ));
    assert!(
        prose.contains("(cost $2.06 at Anthropic rates, endpoint unknown)"),
        "an unclassifiable target neither reads as Anthropic-priced nor claims \
         to know the endpoint: {prose}",
    );
}

/// The same check against the OTHER shape a provider cache really takes: bars
/// and a plan label instead of balance rows (a wallet provider writes no `bars`
/// and no `plan` at all). Both are real captures, so a reader that assumed one
/// shape reds here rather than in front of a model.
#[test]
fn a_running_check_renders_a_bar_shaped_provider_cache_too() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&crate::profile::Profile::new(
        "bars".to_string(),
        Some("https://api.z.ai/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save third-party profile");
    let cache = crate::profile_cache::profile_cache_path("bars", THIRD_PARTY_CACHE_FILE).unwrap();
    std::fs::write(&cache, crate::testutil::THIRD_PARTY_BARS_CACHE_BYTES).expect("provider cache");
    seed_running("d-bars-0", "bars", now_ms());

    let text = monitor_text("d-bars-0");
    assert!(
        text.contains(
            "; quota: no Anthropic 5h/7d window; provider reports pro: 5h 12.5%, 7d 48%, 30d 3%"
        ),
        "the provider's own bars and plan reach the check: {text}",
    );
}

/// Finding 1: `StreamCapture` held the delegate's live text and the progress
/// stamp, both died with the detached task, and a poll read only the disk file.
/// A heartbeat is what carries them across, so a check must show them.
#[test]
fn a_heartbeat_reaches_a_running_check() {
    let _home = HomeSandbox::new();
    let started_at = now_ms() - 733_000;
    let spec = jobs::RunningSpec {
        started_at,
        ..running_spec("d-beat-0", "DS0", started_at)
    };
    jobs::write_running(&spec).unwrap();
    assert!(
        monitor_text("d-beat-0").contains("no output yet"),
        "before any line arrives the check says so, rather than inventing an age",
    );

    // An EPOCH stamp, the same anchor `started_at` carries: 4s ago.
    jobs::write_heartbeat(
        &spec,
        now_ms() - 4000,
        "clippy clean, 0 warnings. moving on",
    )
    .unwrap();
    let text = monitor_text("d-beat-0");
    let ago = rendered_secs(&text, "last output ");
    assert!(
        (4..=5).contains(&ago),
        "the heartbeat's stamp reaches the check: 4s at the write, so 4 or 5 by \
         the read: {text}",
    );
    assert!(
        text.contains("\"clippy clean, 0 warnings. moving on\""),
        "the heartbeat's tail reaches the check, quoted on its own line: {text}",
    );
}

/// Finding 4: `JobRecord` stored neither deadline, so a poll could not say how
/// close the run sat to either kill. Both are recorded at reserve time now — and
/// a run with the idle leg off must read as HAVING no idle deadline, never as
/// clauth having lost the figure.
#[test]
fn both_deadlines_reach_a_running_check() {
    let _home = HomeSandbox::new();
    let started_at = now_ms() - 700_000;
    jobs::write_running(&jobs::RunningSpec {
        started_at,
        timeout_secs: 3600,
        idle_secs: Some(300),
        ..running_spec("d-dl-0", "work", started_at)
    })
    .unwrap();
    let text = monitor_text("d-dl-0");
    let wall = rendered_secs(&text, "wall-kill in ");
    assert!(
        (2899..=2900).contains(&wall),
        "the wall clock counts down from the recorded 3600s ceiling, 700s in: {text}",
    );
    assert!(
        text.contains("idle-kill in 0s"),
        "with no output yet the idle clock has run since the start: {text}",
    );

    // A caller-pinned `--output-format` turns the idle leg off, so clauth knows
    // there IS no such deadline.
    jobs::write_running(&jobs::RunningSpec {
        started_at,
        timeout_secs: 900,
        idle_secs: None,
        ..running_spec("d-dl-1", "work", started_at)
    })
    .unwrap();
    let text = monitor_text("d-dl-1");
    assert!(
        text.contains("no idle deadline") && !text.contains("idle-kill"),
        "a structurally-absent idle deadline reads as none, never unknown: {text}",
    );
    let wall = rendered_secs(&text, "wall-kill in ");
    assert!(
        (199..=200).contains(&wall),
        "the wall clock is still recorded, counting down from 900s: {text}",
    );

    // A job file from a server that recorded neither says so, rather than
    // rendering a zero countdown off a defaulted field.
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-dl-2.json"),
        format!(
            r#"{{"job_id":"d-dl-2","profile":"work","state":"running","started_at":{}}}"#,
            now_ms()
        ),
    )
    .unwrap();
    let text = monitor_text("d-dl-2");
    assert!(
        text.contains("liveness not recorded"),
        "a pre-slice-2 record names the gap instead of counting down from zero: {text}",
    );
    assert!(
        !text.contains("wall-kill") && !text.contains("idle-kill"),
        "no deadline is invented for a record that carries none: {text}",
    );
}

/// `all` is what makes `any` mean anything, and no test made it wait: a batch
/// with nothing done cannot tell the two modes apart, because the early break
/// never arms. Seed one landed lane and one live one, and the modes diverge.
#[test]
fn return_on_all_waits_for_the_slowest_lane_and_any_does_not() {
    let _home = HomeSandbox::new();
    jobs::write_done(
        "d-mode-done-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "first"}),
    )
    .unwrap();
    seed_running("d-mode-slow-0", "work", now_ms());
    let ids = ["d-mode-done-0".to_string(), "d-mode-slow-0".to_string()];

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");

    let start = std::time::Instant::now();
    let any = rt.block_on(async {
        wait_for_batch(&ids, 3, ReturnOn::Any, &mut ProgressSink::none()).await
    });
    let any_elapsed = start.elapsed();
    assert!(
        any_elapsed < std::time::Duration::from_secs(2),
        "`any` returns on the landed lane, not the deadline: {any_elapsed:?}",
    );
    assert!(matches!(&any[1].1, WaitOutcome::Running(_)));

    let start = std::time::Instant::now();
    let all = rt.block_on(async {
        wait_for_batch(&ids, 3, ReturnOn::All, &mut ProgressSink::none()).await
    });
    let all_elapsed = start.elapsed();
    assert!(
        all_elapsed >= std::time::Duration::from_secs(3),
        "`all` waits out the slow lane's deadline: {all_elapsed:?}",
    );
    assert!(matches!(&all[0].1, WaitOutcome::Done(_)));
    assert!(matches!(&all[1].1, WaitOutcome::Running(_)));
}

/// A client that abandons a call sends `notifications/cancelled`, and rmcp
/// cancels `RequestContext.ct` — it does NOT abort the handler future, which it
/// awaits bare (rmcp 3.1.2 `service.rs`). So every wait loop has to race its own
/// sleep against the token, or an abandoned `monitor` leaks for the full ceiling
/// this slice raised to an hour, emitting notifications at a torn-down request
/// id the whole time.
#[test]
fn a_cancelled_request_ends_every_wait_loop_early() {
    let _home = HomeSandbox::new();
    seed_running("d-cancel-0", "work", now_ms());
    let ids = ["d-cancel-0".to_string()];
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");

    // The cancel lands from OUTSIDE, part-way into a wait that is already
    // running, which is both the real sequence (the client abandons a call in
    // flight) and the only shape that reds in bounded time. Pre-cancelling and
    // leaning on an outer `tokio::time::timeout` cannot work: with the guards
    // removed, an already-cancelled token makes `sleep_or_cancelled` return on
    // first poll, `tick` returns before its only await with no channel, and
    // `jobs::read` is sync — so the mutated body has no `Pending` point at all,
    // nothing can preempt a future that never yields, and the regression hangs
    // rather than failing. Measured: 30s wall at ~100% of one core, on a
    // multi-thread runtime too. Cancelled mid-sleep instead, the mutated loop
    // just keeps taking its normal 200ms slices to the deadline below, and the
    // elapsed assertion reds there.
    let deadline_secs = 10;
    let bound = std::time::Duration::from_secs(3);
    let cancel_soon = |sink: &ProgressSink| {
        let ct = sink.cancel_token();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            ct.cancel();
        })
    };

    let mut sink = ProgressSink::none();
    let canceller = cancel_soon(&sink);
    let start = std::time::Instant::now();
    let one = rt.block_on(wait_for_done("d-cancel-0", deadline_secs, &mut sink));
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");
    assert!(
        elapsed < bound,
        "wait_for_done must end on the cancel, not its {deadline_secs}s deadline: {elapsed:?}",
    );
    assert!(
        matches!(one, WaitOutcome::Running(_)),
        "a cancel reads as the deadline arriving: the job is still running",
    );

    let mut sink = ProgressSink::none();
    let canceller = cancel_soon(&sink);
    let start = std::time::Instant::now();
    let batch = rt.block_on(wait_for_batch(
        &ids,
        deadline_secs,
        ReturnOn::All,
        &mut sink,
    ));
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");
    assert!(
        elapsed < bound,
        "wait_for_batch must end on the cancel, not its {deadline_secs}s deadline: {elapsed:?}",
    );
    assert!(matches!(&batch[0].1, WaitOutcome::Running(_)));

    let mut sink = ProgressSink::none();
    let canceller = cancel_soon(&sink);
    let tracker = DigestTracker::new();
    // Seed the baseline so the loop is in its comparing state, not its arming
    // one — the arming shortcut would end the wait for the wrong reason.
    let _ = tracker.report(WatchSet::ALL);
    let start = std::time::Instant::now();
    let watched = rt.block_on(tracker.watch(WatchSet::ALL, deadline_secs, &mut sink));
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");
    assert!(
        elapsed < bound,
        "DigestTracker::watch must end on the cancel, not its {deadline_secs}s deadline: \
         {elapsed:?}",
    );
    assert!(matches!(watched, WatchOutcome::Unchanged { .. }));
}

/// `write_heartbeat`'s lock-free safety rests on the stdout reader thread being
/// JOINED before any envelope is built, so the last heartbeat strictly precedes
/// `write_done`. An early `return` between the spawn and the join orphans the
/// thread: the child keeps writing (`Child::drop` does not kill), the orphan
/// keeps heartbeating, and it overwrites the finalized record with
/// `state: running, envelope: None` — a job that polls running for 70 minutes
/// and an `mcp-await-job` blocked on a terminal state that never arrives.
///
/// The precondition is a `waitpid` failure, which has no practical repro, so the
/// guarantee is structural: there is exactly one exit between those two points.
/// A doc comment asserting it would be a convention, not a guarantee.
///
/// It rejects a bare `?` rather than only `?;`, because `?` early-returns in
/// every spelling it takes (`foo()?;`, `let x = foo()?.bar()`, `Some(foo()?)`)
/// and this guard is the only thing carrying the invariant. A `?` local to a
/// closure in that window would be a false positive; there is none today, and
/// the right answer to one is to restructure it rather than to loosen this,
/// since a reader cannot tell the two apart at a glance either.
#[test]
fn run_delegate_never_returns_between_spawning_the_reader_and_joining_it() {
    let src = include_str!("../../src/mcp/mod.rs");
    let body = src
        .split_once("fn run_delegate(")
        .expect("run_delegate is defined")
        .1;
    let spawn = body
        .find("let stdout_reader = child.stdout.take()")
        .expect("the reader thread is spawned");
    let join = body
        .find("let capture = stdout_reader")
        .expect("the reader thread is joined");
    assert!(spawn < join, "the join follows the spawn");
    let window: String = body[spawn..join]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for shape in ["return ", "return;", "?"] {
        assert!(
            !window.contains(shape),
            "an early exit ({shape:?}) between the reader spawn and its join orphans \
             the thread, and the orphan then overwrites the finalized job file: {window}",
        );
    }
}

/// The mode seam the `watch` fold created is refused by name at the boundary,
/// per placement rule 4 — a rule the server refuses does not have to be taught
/// in the description.
///
/// `cancel`'s own half of that seam moved to
/// `monitor_refuses_cancel_without_job_ids` when the parameter started working:
/// naming it beside `job_ids` is now an ordinary call.
#[test]
fn monitor_refuses_a_cross_mode_return_on() {
    let _home = HomeSandbox::new();
    let refusal = |args: MonitorArgs| {
        let result = call_monitor_args(args);
        assert_eq!(result.is_error, Some(true), "a cross-mode call is refused");
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("refusal text")
    };

    let text = refusal(MonitorArgs {
        job_ids: None,
        wait_secs: None,
        return_on: Some("any".to_string()),
        cancel: None,
    });
    assert!(
        text.contains("`return_on`") && text.contains("`job_ids`"),
        "return_on without job_ids names both halves of the seam: {text}",
    );

    let text = refusal(MonitorArgs {
        job_ids: Some(vec!["d-1-0".to_string()]),
        wait_secs: None,
        return_on: Some("first".to_string()),
        cancel: None,
    });
    assert_eq!(
        text, "error: unrecognized return_on \"first\": accepted \"any\" and \"all\"",
        "a typo names the accepted values, mirroring the `scope` refusal",
    );

    // `cancel: false` is what an absent parameter means, so it changes nothing.
    let ok = call_monitor_args(MonitorArgs {
        job_ids: Some(vec!["d-1-0".to_string()]),
        wait_secs: None,
        return_on: Some("all".to_string()),
        cancel: Some(false),
    });
    let text = ok
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("text");
    assert!(
        text.starts_with("error: unknown job_id"),
        "an explicit `cancel: false` behaves as today: {text}",
    );
}

#[test]
fn monitor_done_returns_envelope_and_evicts() {
    let _home = HomeSandbox::new();
    let env = serde_json::json!({ "profile": "work", "is_error": false, "result": "all done" });
    jobs::write_done("d-done-0", "work", 1, env).unwrap();

    let result = call_monitor("d-done-0", Some(0));
    assert_ne!(result.is_error, Some(true));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("envelope text");
    assert!(text.contains("all done"), "envelope result delivered");
    assert!(
        jobs::read("d-done-0").is_none(),
        "done job evicted on fetch"
    );
}

/// A delegate whose `claude -p` printed a bare JSON scalar (`parse_delegate_envelope`'s
/// fall-through arm returns non-objects verbatim) must not panic the fold, and
/// its own output must reach the caller.
#[test]
fn monitor_done_scalar_envelope_is_wrapped_not_panicked() {
    let _home = HomeSandbox::new();
    jobs::write_done("d-scalar-0", "work", 1, serde_json::json!("unauthorized")).unwrap();

    let result = call_monitor("d-scalar-0", Some(0));
    assert_ne!(
        result.is_error,
        Some(true),
        "a scalar self-report is delivered, not an error"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("envelope text");
    assert!(
        text.contains("delegate to `work` finished: unauthorized"),
        "the delegate's own output survives the fold: {text}",
    );
    assert!(
        text.contains("; target `work`: 5h"),
        "live usage still folds in around the wrapped result",
    );
    assert!(
        jobs::read("d-scalar-0").is_none(),
        "the delivered job is evicted"
    );
}

/// The eviction must run only after the envelope rendered: a panic between the
/// two (the pre-fix scalar fold) destroyed the job file, the only surviving
/// copy of the delegate's result.
#[test]
fn monitor_done_keeps_the_job_until_the_result_renders() {
    let _home = HomeSandbox::new();
    jobs::write_done("d-keep-0", "work", 1, serde_json::json!(42)).unwrap();

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        call_monitor("d-keep-0", Some(0))
    }));
    match outcome {
        Ok(result) => {
            let text = result
                .content
                .first()
                .and_then(|c| c.as_text())
                .map(|t| t.text.clone())
                .expect("envelope text");
            assert!(
                text.contains("finished: 42"),
                "a numeric self-report survives the fold: {text}",
            );
            assert!(
                jobs::read("d-keep-0").is_none(),
                "the delivered job is evicted"
            );
        }
        Err(_) => {
            assert!(
                jobs::read("d-keep-0").is_some(),
                "a failed render must leave the job file as the recoverable copy"
            );
        }
    }
}

/// The done-arm renderer is pure of the job store: it renders without evicting,
/// and the handler evicts only after it returned. An eviction moved inside the
/// renderer would destroy the only copy before the envelope was safely out.
#[test]
fn render_done_envelope_leaves_the_job_until_the_caller_evicts() {
    let _home = HomeSandbox::new();
    jobs::write_done("d-render-0", "work", 1, serde_json::json!("unauthorized")).unwrap();
    let record = jobs::read("d-render-0").expect("seeded job");

    let (blocks, is_error) = render_done_envelope(record, &DigestTracker::new());

    assert!(
        !is_error,
        "a scalar self-report renders as a delivery, not an error"
    );
    assert!(
        jobs::read("d-render-0").is_some(),
        "rendering never evicts; the handler does, after it returned"
    );
    let text = blocks
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("block text");
    assert!(
        text.contains("delegate to `work` finished: unauthorized"),
        "the delegate's own output survives the fold: {text}",
    );
}

#[test]
fn monitor_batch_returns_one_result_per_id_in_order() {
    let _home = HomeSandbox::new();
    jobs::write_done(
        "d-b1-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "all done"}),
    )
    .unwrap();
    seed_running("d-b2-0", "work", now_ms());

    let result = call_monitor_batch(vec!["d-b1-0", "d-b2-0", "d-b3-0"], Some(0));
    assert_ne!(
        result.is_error,
        Some(true),
        "a batch with an absent id is not an error"
    );
    assert_eq!(
        result.content.len(),
        1,
        "the batch is a single content block"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("results text");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "one line per requested id: {text}");

    assert_eq!(
        lines[0], "job `d-b1-0` finished: all done",
        "a done id carries its envelope, in the order given",
    );
    assert!(
        jobs::read("d-b1-0").is_none(),
        "a done job is evicted on batch fetch"
    );

    assert!(
        lines[1].starts_with("job `d-b2-0` running on `work`, elapsed"),
        "a running line names its id, state and elapsed: {text}",
    );
    assert!(
        jobs::read("d-b2-0").is_some(),
        "a running job is not evicted"
    );

    assert_eq!(
        lines[2], "job `d-b3-0` unknown",
        "an absent id is reported per-id, never dropped"
    );
}

#[test]
fn monitor_batch_prose_is_one_block_with_one_line_per_job() {
    let _home = HomeSandbox::new();
    // The done result is multi-line on purpose: real delegate output wraps,
    // and a single-line fixture would let the line count pass for the wrong
    // reason (the count would pin the fixture, not the per-job shape).
    jobs::write_done(
        "d-b1-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "line one\nline two"}),
    )
    .unwrap();
    // The running job carries a tail: it is the one thing in the running line
    // that adds a line of its own, so a tail-less fixture would pin the count
    // against the only shape that cannot break it.
    let spec = running_spec("d-b2-0", "work", now_ms());
    jobs::write_running(&spec).unwrap();
    jobs::write_heartbeat(&spec, now_ms(), "still compiling").unwrap();

    let result = call_monitor_batch(vec!["d-b1-0", "d-b2-0", "d-b3-0"], Some(0));
    assert_ne!(result.is_error, Some(true));
    assert_eq!(result.content.len(), 1, "prose is a single content block");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("prose text");
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "the prose default must not be a JSON blob"
    );
    let lines: Vec<&str> = text.split('\n').collect();
    let named: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("job `"))
        .collect();
    assert_eq!(
        lines.len(),
        5,
        "three named job lines, one wrapped line of the multi-line result, and \
         the running job's own quoted tail line"
    );
    assert_eq!(
        lines[3], "    \"still compiling\"",
        "a tail rides its own indented line under its job",
    );
    assert_eq!(named.len(), 3, "one named line per job");
    assert_eq!(named[0], "job `d-b1-0` finished: line one");
    assert_eq!(
        lines[1], "line two",
        "a multi-line result wraps inside its own job line"
    );
    assert!(
        named[1].starts_with("job `d-b2-0` running on `work`, elapsed"),
        "a running line names its id and state: {lines:?}"
    );
    assert_eq!(named[2], "job `d-b3-0` unknown");
}

/// A batch scan whose deadline passes mid-pass must resolve every id checked
/// before the crossing by its own state: a still-running file reads
/// `running`, never the byte-identical `unknown` of an absent id. The scan
/// straddles the 1s deadline deterministically: the running id sits first
/// and enough absent ids trail it that one pass of the loop outlasts the
/// deadline, so the crossing lands between the running id's check and the
/// loop-bottom deadline test. CPU contention only lengthens the pass.
///
/// Both `return_on` modes are driven: `any` gained an early break, which is
/// exactly the shape that could drop an unresolved slot out as `unknown`.
#[test]
fn wait_for_batch_running_id_never_falls_out_as_unknown_at_deadline() {
    let _home = HomeSandbox::new();
    seed_running("d-race-0", "work", now_ms());

    let trailing = 1_000_000;
    let mut ids = Vec::with_capacity(trailing + 1);
    ids.push("d-race-0".to_string());
    ids.extend((1..=trailing).map(|i| format!("d-race-{i}")));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    for mode in [ReturnOn::Any, ReturnOn::All] {
        let outcomes =
            rt.block_on(async { wait_for_batch(&ids, 1, mode, &mut ProgressSink::none()).await });
        assert!(
            matches!(&outcomes[0].1, WaitOutcome::Running(_)),
            "a still-running id at the deadline resolves running, never unknown ({mode:?})"
        );
        assert!(
            matches!(&outcomes[1].1, WaitOutcome::Unknown),
            "an absent id resolves unknown, so the scan really read the list ({mode:?})"
        );
    }
}

/// A collect must never destroy what it came for. The Done TTL is measured from
/// the FINISH, and the sweep a collect runs reaps only orphaned `running` files
/// — a mint-anchored done sweep here deletes the salvage envelope of every
/// delegate killed at the default 3600 s wall clock, on the very next check,
/// and tells the caller to spend another window.
#[test]
fn a_collect_never_sweeps_the_envelope_it_came_for() {
    let _home = HomeSandbox::new();
    // Minted two hours ago, finalized a moment ago: the shape of any long run.
    let minted = now_ms() - 2 * 60 * 60 * 1000;
    jobs::write_done(
        "d-salvage-0",
        "work",
        minted,
        serde_json::json!({
            "profile": "work",
            "is_error": true,
            "timed_out": "wall_clock",
            "result": "delegate killed at its 3600s wall-clock ceiling",
            "partial_result": "the answer it had already written",
        }),
    )
    .unwrap();
    jobs::write_done(
        "d-bystander-0",
        "work",
        minted,
        serde_json::json!({"profile": "work", "is_error": false, "result": "someone else's answer"}),
    )
    .unwrap();

    // A result that finished over the retention TTL ago is the case the two
    // sweeps disagree on: startup is entitled to reap it, a reader never is.
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-late-0.json"),
        format!(
            r#"{{"job_id":"d-late-0","profile":"work","state":"done","started_at":{minted},
               "done_at":{minted},"envelope":{{"result":"collected late"}}}}"#
        ),
    )
    .unwrap();

    // A check on an unrelated, absent id must not reap a bystander's result
    // either — the sweep runs before the read on every job-mode call.
    let _ = call_monitor("d-1-0", Some(0));
    assert!(
        jobs::read("d-bystander-0").is_some(),
        "a check on one id must not destroy another job's result",
    );
    assert!(
        jobs::read("d-late-0").is_some(),
        "a reader never applies the done TTL: that is startup's call, and here it \
         would delete a result nobody has read yet",
    );

    let text = call_monitor("d-salvage-0", Some(0))
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("envelope text");
    assert!(
        text.contains("the answer it had already written"),
        "the salvage envelope is delivered, not swept: {text}",
    );
}

/// `return_on: "any"` exists so a fan-out poll reacts to whichever lane lands
/// first instead of paying the slowest lane's wait. The slow lane must still
/// resolve by its own state — an early break is the shape that could drop it
/// out as `unknown`.
#[test]
fn return_on_any_returns_before_the_slowest_lane() {
    let _home = HomeSandbox::new();
    jobs::write_done(
        "d-fast-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "first"}),
    )
    .unwrap();
    seed_running("d-slow-0", "work", now_ms());

    let start = std::time::Instant::now();
    let result = call_monitor_args(MonitorArgs {
        job_ids: Some(vec!["d-fast-0".to_string(), "d-slow-0".to_string()]),
        wait_secs: Some(30),
        return_on: Some("any".to_string()),
        cancel: None,
    });
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "one landed lane ends the wait; took {elapsed:?} of a 30s budget",
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("results text");
    assert!(
        text.contains("job `d-fast-0` finished: first"),
        "the landed lane carries its envelope: {text}",
    );
    assert!(
        text.contains("job `d-slow-0` running"),
        "the lane still in flight reads running, never unknown: {text}",
    );
}

#[test]
fn monitor_batch_failed_job_is_a_protocol_error() {
    let _home = HomeSandbox::new();
    jobs::write_done(
        "d-fail-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": true, "result": "boom"}),
    )
    .unwrap();
    jobs::write_done(
        "d-ok-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "fine"}),
    )
    .unwrap();

    let result = call_monitor_batch(vec!["d-fail-0", "d-ok-0"], Some(0));
    assert_eq!(
        result.is_error,
        Some(true),
        "any failed job makes the batch a protocol-level error"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("results text");
    assert!(
        text.contains("job `d-fail-0` failed: boom"),
        "the per-result verdict survives inside the content: {text}",
    );
    assert!(
        text.contains("job `d-ok-0` finished: fine"),
        "an ok result keeps its own verdict: {text}",
    );
    assert!(
        jobs::read("d-fail-0").is_none(),
        "a failed job is still evicted"
    );
    assert!(jobs::read("d-ok-0").is_none(), "an ok job is still evicted");

    jobs::write_done(
        "d-ok2-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "fine"}),
    )
    .unwrap();
    let ok_only = call_monitor_batch(vec!["d-ok2-0"], Some(0));
    assert_eq!(
        ok_only.is_error,
        Some(false),
        "an all-ok batch is a protocol-level success"
    );
}

#[test]
fn monitor_batch_never_evicts_a_mismatched_stored_job_id() {
    let _home = HomeSandbox::new();
    // The unrelated file the stored `job_id` names: it must survive a fetch
    // of the mismatched file. With the pre-fix code the batch evicted by the
    // stored id, deleting whatever path the hand-written file pointed at.
    jobs::write_done(
        "d-decoy-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "decoy"}),
    )
    .unwrap();
    // A hand-written job file whose stored `job_id` disagrees with its
    // filename, a shape `jobs::read` deserializes without complaint.
    let dir = jobs::jobs_dir().expect("jobs dir");
    let record = serde_json::json!({
        "job_id": "d-decoy-0",
        "profile": "work",
        "state": "done",
        "started_at": 1,
        "envelope": {"profile": "work", "is_error": false, "result": "stolen"},
    });
    std::fs::write(
        dir.join("d-mis-0.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();

    // A second, absent id forces the several-ids path; one id alone renders
    // through the single-job spelling.
    let result = call_monitor_batch(vec!["d-mis-0", "d-absent-0"], Some(0));
    assert_eq!(
        result.is_error,
        Some(false),
        "a delivered mismatched file is not an error"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("results text");
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        vec!["job `d-mis-0` finished: stolen", "job `d-absent-0` unknown"],
        "the result is reported under the caller-supplied id, envelope intact",
    );
    assert!(
        jobs::read("d-decoy-0").is_some(),
        "the file the stored id names is never evicted"
    );
    assert!(
        jobs::read("d-mis-0").is_some(),
        "the mismatched file itself is not evicted"
    );
}

#[test]
fn monitor_batch_refuses_over_cap_and_empty_list() {
    let _home = HomeSandbox::new();
    let over = call_monitor_args(MonitorArgs {
        job_ids: Some((0..257).map(|i| format!("d-{i}")).collect()),
        wait_secs: None,
        return_on: None,
        cancel: None,
    });
    assert_eq!(over.is_error, Some(true), "a list over the cap is refused");
    assert_eq!(
        over.content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("refusal text"),
        "error: `job_ids` capped at 256 ids; got 257"
    );

    // An empty `job_ids` is job mode with no ids, not the state-waiting mode:
    // omitting the parameter entirely is what asks about clauth's state.
    let empty = call_monitor_args(MonitorArgs {
        job_ids: Some(Vec::new()),
        wait_secs: None,
        return_on: None,
        cancel: None,
    });
    assert_eq!(empty.is_error, Some(true), "an empty list is refused");
    assert_eq!(
        empty
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("refusal text"),
        "error: `job_ids` is empty: name at least one job_id"
    );
}

/// The one-id spelling, pinned verbatim: a several-ids caller must never change
/// what an existing one-id caller sees. The done arm is byte-identical to the
/// pre-merge tool; the unknown arm deliberately is not — it keeps the pre-merge
/// lead and id and appends the cause, which `unknown job_id` alone never
/// carried.
#[test]
fn monitor_single_spelling_keeps_the_pre_merge_done_bytes_and_names_its_unknown_cause() {
    let _home = HomeSandbox::new();

    let unknown = call_monitor("d-pin-unknown-0", Some(0));
    assert_eq!(unknown.is_error, Some(true));
    // The lead and the id are the pre-merge bytes; what follows is the named
    // cause, which `unknown job_id` alone never carried.
    assert_eq!(
        unknown
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("refusal text"),
        "error: unknown job_id: d-pin-unknown-0 — clauth never minted it (a real one reads \
         `d-<epoch_ms>-<counter>`); check the id `delegate` handed back"
    );

    jobs::write_done(
        "d-pin-done-0",
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "all done"}),
    )
    .unwrap();
    let done = call_monitor("d-pin-done-0", Some(0));
    assert_eq!(
        done.content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("done text"),
        "delegate to `work` finished: all done; target `work`: 5h unknown, 7d unknown"
    );
}

/// Every non-object envelope shape folds without panicking and keeps the
/// delegate's own output verbatim under `result`.
#[test]
fn fold_delegate_live_usage_wraps_non_objects_and_folds_objects() {
    let _home = HomeSandbox::new();
    let digest = DigestTracker::new();
    for scalar in [
        serde_json::json!("unauthorized"),
        serde_json::json!(42),
        serde_json::json!(true),
        serde_json::json!([1, 2]),
    ] {
        let folded =
            fold_delegate_live_usage(scalar.clone(), "work", 0, DigestMode::Report(&digest));
        let obj = folded.as_object().expect("a folded envelope is an object");
        assert_eq!(
            obj.get("result"),
            Some(&scalar),
            "the delegate's own output survives the fold verbatim"
        );
        assert!(obj.get("live_usage").is_some(), "live usage folds in");
    }

    // An object envelope folds in place and keeps its own fields.
    let folded = fold_delegate_live_usage(
        serde_json::json!({"profile": "work", "is_error": false, "result": "all done"}),
        "work",
        0,
        DigestMode::Report(&digest),
    );
    assert_eq!(folded["result"], "all done");
    assert_eq!(folded["live_usage"]["profile"], "work");
    assert!(
        folded.get("is_error").is_some(),
        "the envelope's own fields survive"
    );
}

/// Finding 9: the MCP server runs no scheduler, so with no daemon its cache is
/// arbitrarily old — and every figure it reported was undated, which is a
/// routing decision made on a number of unknown age. A folded figure now carries
/// how old it is, and one past any refresh cadence still carries its number
/// rather than being dropped for a `unknown` that reads as a lost account.
#[test]
fn a_folded_live_usage_clause_dates_the_figure_it_carries() {
    let _home = HomeSandbox::new();
    let usage = UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 12.0,
            resets_at: None,
        }),
        ..Default::default()
    };
    let cache_path =
        crate::profile_cache::profile_cache_path("work", USAGE_CACHE_FILE).expect("cache path");

    crate::profile_cache::write_profile_cache("work", USAGE_CACHE_FILE, &usage);
    crate::testutil::set_mtime(
        &cache_path,
        std::time::SystemTime::now() - Duration::from_secs(240),
    );
    let fresh = render::delegate_prose(&fold_delegate_live_usage(
        serde_json::json!({"is_error": false, "result": "ok"}),
        "work",
        0,
        DigestMode::Skip,
    ));
    assert!(
        fresh.contains("target `work`: 5h 12% used, 7d unknown (cached 4m ago)"),
        "the figure names the age of the cache it came from: {fresh}",
    );

    // Past the longest gap a live scheduler can leave (interval ceiling plus the
    // widen-only backoff ceiling, doubled for the fetch's own latency), so
    // nothing is maintaining this figure — and it still carries its number.
    crate::testutil::set_mtime(
        &cache_path,
        std::time::SystemTime::now() - Duration::from_secs(3 * 60 * 60),
    );
    let stale = render::delegate_prose(&fold_delegate_live_usage(
        serde_json::json!({"is_error": false, "result": "ok"}),
        "work",
        0,
        DigestMode::Skip,
    ));
    assert!(
        stale.contains("5h 12% used, 7d unknown (cached 3h 0m ago, stale)"),
        "a stale figure is dated and marked, never suppressed: {stale}",
    );
}

/// Every non-object payload folds without panicking and keeps the caller's
/// payload verbatim under `result`.
#[test]
fn fold_active_live_usage_wraps_non_objects_and_folds_objects() {
    let _home = HomeSandbox::new();
    let config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    let digest = DigestTracker::new();
    for scalar in [
        serde_json::json!("oops"),
        serde_json::json!(42),
        serde_json::json!([1, 2]),
    ] {
        let folded = fold_active_live_usage(scalar.clone(), &config, DigestMode::Report(&digest));
        let obj = folded.as_object().expect("a folded payload is an object");
        assert_eq!(
            obj.get("result"),
            Some(&scalar),
            "the caller's payload survives the fold verbatim"
        );
        assert!(obj.get("live_usage").is_some(), "live usage folds in");
    }

    // An object payload folds in place and keeps its own fields.
    let folded = fold_active_live_usage(
        serde_json::json!({"ok": true, "previous": "a", "active": "b"}),
        &config,
        DigestMode::Report(&digest),
    );
    assert_eq!(folded["ok"], true);
    assert_eq!(folded["previous"], "a");
    assert!(
        folded.get("live_usage").is_some(),
        "the payload's own fields survive"
    );
}

#[test]
fn background_depth_guard_refuses_without_writing_job() {
    let _home = HomeSandbox::new();
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by HOME_TEST_LOCK (held by the sandbox),
    // restored unconditionally below.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, "1") };

    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let result = rt.block_on(async {
        server
            .delegate(Parameters(DelegateArgs {
                profiles: Some(vec!["any".to_string()]),
                prompt: Some("hello".to_string()),
                prompt_file: None,
                model: None,
                cwd: None,
                env: None,
                args: None,
                timeout_secs: None,
                idle_secs: None,
                resume: None,
                isolated: None,
                background: Some(true),
            }))
            .await
    });

    // SAFETY: restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }

    let result = result.expect("delegate returns a tool result, never a transport error");
    assert_eq!(
        result.is_error,
        Some(true),
        "depth-1 background delegate refuses"
    );
    let job_count = jobs::jobs_dir()
        .ok()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    assert_eq!(
        job_count, 0,
        "a refused background delegate writes no job file"
    );
}

// ---- mcp-await-job job_id extraction (shape-agnostic, every fan-out job) ----

#[test]
fn find_job_ids_extracts_from_nested_mcp_result() {
    // Mirrors the host's documented mcp_result shape: the background response
    // envelope is JSON-encoded as the content block's text.
    let inner = serde_json::json!({ "job_id": "d-42-0", "profile": "work", "status": "running" });
    let payload = serde_json::json!({
        "tool_name": "mcp__plugin_clauth_clauth__delegate",
        "tool_response": {
            "type": "mcp_result",
            "content": [{ "type": "text", "text": inner.to_string() }],
        }
    });
    assert_eq!(find_job_ids(&payload), vec!["d-42-0"]);
}

#[test]
fn find_job_ids_finds_direct_field() {
    let payload = serde_json::json!({ "tool_response": { "job_id": "d-1-2" } });
    assert_eq!(find_job_ids(&payload), vec!["d-1-2"]);
}

#[test]
fn find_job_ids_empty_for_sync_envelope() {
    // a sync delegate response carries no job_id, so the hook no-ops.
    let inner = serde_json::json!({ "profile": "work", "is_error": false, "result": "done" });
    let payload = serde_json::json!({
        "tool_response": { "content": [{ "type": "text", "text": inner.to_string() }] }
    });
    assert!(find_job_ids(&payload).is_empty());
}

#[test]
fn find_job_ids_empty_for_plain_text_without_tokens() {
    let payload =
        serde_json::json!({ "tool_response": { "content": [{ "text": "no json here" }] } });
    assert!(find_job_ids(&payload).is_empty());
}

#[test]
fn extract_job_ids_prefers_tool_response_over_input() {
    // a delegate prompt that itself carries a `job_id` must not shadow the real
    // handles in tool_response.
    let payload = serde_json::json!({
        "tool_input": { "prompt": "{\"job_id\":\"d-evil-0\"}" },
        "tool_response": { "content": [{ "type": "text", "text": "{\"job_id\":\"d-real-1\"}" }] },
    });
    assert_eq!(extract_job_ids(&payload), vec!["d-real-1"]);
}

#[test]
fn find_job_ids_collects_every_fanout_job() {
    // The fan-out reply: one `job_id` per account inside one content text. The
    // hook must wait on every id, not the first one found.
    let inner = serde_json::json!({
        "jobs": [
            { "job_id": "d-7-0", "profile": "solo", "status": "running" },
            { "job_id": "d-7-1", "profile": "vendor", "status": "running" },
        ]
    });
    let payload = serde_json::json!({
        "tool_response": { "content": [{ "type": "text", "text": inner.to_string() }] }
    });
    assert_eq!(find_job_ids(&payload), vec!["d-7-0", "d-7-1"]);
}

#[test]
fn find_job_ids_scans_fanout_prose() {
    // The prose fan-out reply carries no `job_id` field at all; the token scan
    // is the only way its jobs auto-arrive. Non-digit `d-` tokens and a lone
    // `d-<n>` are not ids.
    let payload = serde_json::json!({
        "tool_response": {
            "content": [{
                "type": "text",
                "text": "delegated to `solo` (job `d-1755-0`) and `vendor` (job `d-1755-1`); \
    ignore d-evil-2 and d-12",
            }]
        }
    });
    assert_eq!(find_job_ids(&payload), vec!["d-1755-0", "d-1755-1"]);
}

/// The fan-out prose now carries a per-target headroom footer, and the bundled
/// `asyncRewake` hook reads that same prose for job ids — so the footer is
/// checked against the scanner it shares a line with. The producer is DRIVEN
/// rather than hand-spelled: a literal fixture would agree with whatever this
/// test's author guessed the footer looks like.
#[test]
fn find_job_ids_over_a_footered_fanout_prose_yields_exactly_the_real_ids() {
    let prose = render::delegate_fanout_prose(&serde_json::json!({
        "jobs": [
            {
                "job_id": "d-1755-0",
                "profile": "solo",
                "started_at": 1,
                "status": "running",
                "live_usage": {
                    "profile": "solo",
                    "kind": "oauth",
                    "5h_used_pct": 12.0,
                    "7d_used_pct": 45.6,
                    "fetched_secs_ago": 90,
                },
            },
            {
                "job_id": "d-1755-1",
                "profile": "vendor",
                "started_at": 2,
                "status": "running",
                "live_usage": {
                    "profile": "vendor",
                    "kind": "third_party",
                    "balance": "total: 31.45 USD",
                    "fetched_secs_ago": 30,
                },
            },
        ]
    }));
    assert!(
        prose.contains("5h 12% used") && prose.contains("total: 31.45 USD"),
        "fixture control: the prose really carries both footers: {prose}",
    );
    let payload = serde_json::json!({
        "tool_response": { "content": [{ "type": "text", "text": prose }] }
    });
    assert_eq!(find_job_ids(&payload), vec!["d-1755-0", "d-1755-1"]);
}

#[test]
fn await_job_outcomes_delivers_each_and_drops_absent() {
    let _home = HomeSandbox::new();
    seed_running("d-multi-0", "solo", now_ms());
    seed_running("d-multi-1", "vendor", now_ms());
    jobs::write_done(
        "d-multi-0",
        "solo",
        1,
        serde_json::json!({ "profile": "solo", "is_error": false, "result": "a" }),
    )
    .unwrap();
    jobs::write_done(
        "d-multi-1",
        "vendor",
        1,
        serde_json::json!({ "profile": "vendor", "is_error": false, "result": "b" }),
    )
    .unwrap();

    let (delivered, pending) = await_job_outcomes(
        &[
            "d-multi-0".to_string(),
            "d-multi-1".to_string(),
            "d-gone-9".to_string(),
        ],
        std::time::Duration::from_secs(2),
    );
    assert!(
        pending.is_empty(),
        "every done or absent id leaves the wait set: {pending:?}"
    );
    let results: Vec<&str> = delivered
        .iter()
        .map(|e| e["result"].as_str().expect("result string"))
        .collect();
    assert_eq!(
        results,
        vec!["a", "b"],
        "one envelope per finished job, in input order"
    );
}

#[test]
fn await_job_outcomes_reports_still_running_at_deadline() {
    let _home = HomeSandbox::new();
    seed_running("d-stuck-0", "solo", now_ms());

    let (delivered, pending) = await_job_outcomes(
        &["d-stuck-0".to_string()],
        std::time::Duration::from_millis(300),
    );
    assert!(delivered.is_empty(), "a still-running job delivers nothing");
    assert_eq!(pending, vec!["d-stuck-0"], "the running id is reported");
}

#[test]
fn monitor_long_poll_sees_completion() {
    let _home = HomeSandbox::new();
    seed_running("d-poll-0", "work", now_ms());
    // Finalize the job shortly after the long-poll starts, from another thread.
    // The home override is process-global (set by HomeSandbox), so the writer
    // resolves the same sandbox jobs dir.
    let writer = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let env =
            serde_json::json!({ "profile": "work", "is_error": false, "result": "late finish" });
        jobs::write_done("d-poll-0", "work", 1, env).unwrap();
    });
    let result = call_monitor("d-poll-0", Some(5));
    writer.join().unwrap();

    assert_ne!(result.is_error, Some(true));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("envelope text");
    assert!(
        text.contains("late finish"),
        "long-poll delivers the envelope completed mid-wait"
    );
}

// `parse_delegate_envelope` normalizes whatever `claude` writes to stdout down to
// the single terminal result object. The regression that motivated it: a caller's
// `--verbose` flips `--output-format json` to the full transcript ARRAY, which
// parsed cleanly and got stored/dumped verbatim (~900K of per-token + tool-io
// events for a multi-minute run) instead of the ~1K envelope.

#[test]
fn parse_envelope_passes_plain_json_object_through() {
    let stdout = r#"{"type":"result","is_error":false,"result":"ok","total_cost_usd":0.01}"#;
    let env = super::parse_delegate_envelope(stdout).expect("plain object parses");
    assert_eq!(env["result"], "ok");
    assert_eq!(env["is_error"], false);
}

#[test]
fn parse_envelope_collapses_verbose_transcript_array() {
    // `--output-format json --verbose`: a leading 10KB+ `system` event, an
    // `assistant` turn, then the terminal `result`. Only the result must survive.
    let stdout = r#"[
        {"type":"system","subtype":"init","blob":"AAAAAAAAAAAAAAAAAAAA"},
        {"type":"assistant","message":{"content":[{"type":"thinking","thinking":"x"}]}},
        {"type":"result","is_error":false,"result":"final report","total_cost_usd":0.5}
    ]"#;
    let env = super::parse_delegate_envelope(stdout).expect("array collapses");
    assert!(
        env.is_object(),
        "envelope is the result object, not the array"
    );
    assert_eq!(env["result"], "final report");
    assert!(env.get("blob").is_none(), "transcript noise is dropped");
    assert!(env.get("thinking").is_none());
}

#[test]
fn parse_envelope_recovers_result_from_ndjson_stream() {
    // `--output-format stream-json`: newline-delimited events, not a single value.
    let stdout = "{\"type\":\"system\",\"subtype\":\"thinking_tokens\",\"estimated_tokens\":1}\n\
                  {\"type\":\"assistant\"}\n\
                  {\"type\":\"result\",\"is_error\":false,\"result\":\"streamed\"}";
    let env = super::parse_delegate_envelope(stdout).expect("ndjson recovers result");
    assert_eq!(env["result"], "streamed");
}

#[test]
fn parse_envelope_errors_on_unparseable_output() {
    let err = super::parse_delegate_envelope("not json at all").expect_err("garbage is an error");
    assert!(err.contains("failed to parse claude output"));
}

// ── delegate deadlines ───────────────────────────────────────────────────────

// The regression these pin: a wall-clock-only deadline cannot see whether the
// child is producing anything, so a delegate mid-answer was killed at 300s
// exactly like a hung one, and its output (already paid for in the target
// account's window) was thrown away with it.

#[test]
fn a_delegate_still_streaming_outlives_the_old_wall_clock() {
    // 50 minutes in, last event a second ago: working, so nothing fires.
    assert_eq!(
        super::expiry(
            Duration::from_secs(3000),
            Duration::from_secs(2999),
            Duration::from_secs(3600),
            Duration::from_secs(300),
            true,
        ),
        None
    );
}

#[test]
fn silence_past_the_idle_window_kills_the_delegate() {
    assert_eq!(
        super::expiry(
            Duration::from_secs(400),
            Duration::from_secs(50),
            Duration::from_secs(3600),
            Duration::from_secs(300),
            true,
        ),
        Some(super::Expiry::Idle)
    );
}

#[test]
fn the_wall_clock_still_bounds_a_delegate_that_never_goes_quiet() {
    assert_eq!(
        super::expiry(
            Duration::from_secs(3600),
            Duration::from_secs(3599),
            Duration::from_secs(3600),
            Duration::from_secs(300),
            true,
        ),
        Some(super::Expiry::Wall)
    );
    // Stalled AT the ceiling trips both legs in one poll. The wall clock is the
    // outer bound, so that is the reason reported.
    assert_eq!(
        super::expiry(
            Duration::from_secs(3600),
            Duration::from_secs(100),
            Duration::from_secs(3600),
            Duration::from_secs(300),
            true,
        ),
        Some(super::Expiry::Wall)
    );
}

/// A caller-pinned `--output-format` means no event stream, so silence carries
/// no information and only the wall clock may fire.
#[test]
fn without_the_stream_silence_never_kills() {
    // Silent from the first second, well past the idle window: only the wall
    // clock may end it.
    let quiet_forever = |elapsed| {
        super::expiry(
            Duration::from_secs(elapsed),
            Duration::from_secs(0),
            Duration::from_secs(1800),
            Duration::from_secs(300),
            false,
        )
    };
    assert_eq!(quiet_forever(1799), None);
    assert_eq!(quiet_forever(1800), Some(super::Expiry::Wall));
}

#[test]
fn deadline_defaults_follow_whether_the_child_streams() {
    // Streaming: the idle deadline governs, the wall clock is the hour backstop.
    assert_eq!(
        super::resolve_deadlines(None, None, true),
        (Duration::from_secs(3600), Duration::from_secs(300))
    );
    // Not streaming: an unset wall clock drops to the idle default so a hung
    // child never sits for the full hour unwatched.
    assert_eq!(
        super::resolve_deadlines(None, None, false),
        (Duration::from_secs(300), Duration::from_secs(300))
    );
}

#[test]
fn caller_deadlines_clamp_to_the_supported_range() {
    let (wall, idle) = super::resolve_deadlines(Some(99_999), Some(0), true);
    assert_eq!(wall, Duration::from_secs(3600));
    assert_eq!(idle, Duration::from_secs(1));
}

#[test]
fn a_pinned_output_format_is_recognized_in_both_spellings() {
    assert!(super::sets_output_format(&[
        "--output-format".to_string(),
        "json".to_string()
    ]));
    assert!(super::sets_output_format(&[
        "--output-format=stream-json".to_string()
    ]));
    assert!(!super::sets_output_format(&[
        "--verbose".to_string(),
        "--model".to_string(),
        "haiku".to_string()
    ]));
}

// ── streamed-output capture + salvage ────────────────────────────────────────

/// One NDJSON event per line, in the order a real `claude -p --output-format
/// stream-json --verbose --include-partial-messages` emits them: a thinking
/// delta, then the deltas of a text block, then the completed `assistant`
/// message carrying that same text, then the terminal envelope.
const STREAM: &str = concat!(
    r#"{"type":"system","subtype":"init","session_id":"s1","blob":"AAAAAAAAAAAA"}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing it"}}}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"al"}}}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pha"}}}"#,
    "\n",
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"alpha"}]}}"#,
    "\n",
    r#"{"type":"result","is_error":false,"result":"final report","total_cost_usd":0.5}"#,
    "\n",
    // A Stop hook fires after the envelope, so the terminal result is not
    // reliably the last line on the wire.
    r#"{"type":"system","subtype":"hook_response","hook_event":"Stop","exit_code":0}"#,
    "\n",
);

#[test]
fn a_finished_stream_yields_only_its_terminal_envelope() {
    let mut capture = super::StreamCapture::default();
    for line in STREAM.lines() {
        capture.push_line(line);
    }
    let envelope = super::parse_delegate_envelope(capture.envelope_src()).expect("result parses");
    assert_eq!(envelope["result"], "final report");
    assert!(
        envelope.get("blob").is_none(),
        "transcript noise never reaches the caller"
    );
}

/// The deltas and the completed block carry the same text. Counting both would
/// hand a killed delegate's salvage back doubled.
#[test]
fn streamed_deltas_are_not_counted_twice_against_their_own_block() {
    let mut capture = super::StreamCapture::default();
    for line in STREAM.lines() {
        capture.push_line(line);
    }
    assert_eq!(capture.partial_text(), "alpha");
}

/// The case the whole salvage exists for: killed mid-block, so only deltas
/// arrived and no `assistant` event ever completed them.
#[test]
fn a_delegate_killed_mid_block_still_returns_the_text_it_wrote() {
    let mut capture = super::StreamCapture::default();
    for line in STREAM.lines().take(4) {
        capture.push_line(line);
    }
    assert_eq!(
        capture.partial_text(),
        "alpha",
        "the salvage is the answer, not the reasoning"
    );
    assert!(
        capture.envelope_src().contains(r#""subtype":"init""#),
        "the fallback report shows a real event, never a token delta: {}",
        capture.envelope_src()
    );
    let envelope = super::timeout_envelope(
        "work",
        super::Expiry::Idle,
        Duration::from_secs(612),
        Duration::from_secs(300),
        &capture,
        true,
    );
    assert_eq!(envelope["is_error"], true);
    assert_eq!(envelope["timed_out"], "idle");
    assert_eq!(envelope["elapsed_secs"], 612);
    assert_eq!(envelope["partial_result"], "alpha");
    assert_eq!(
        envelope["session_id"], "s1",
        "the handle a resume needs comes off the stream, not the envelope the run never reached"
    );
    assert!(
        envelope["result"]
            .as_str()
            .expect("reason")
            .contains("no output for 300s"),
        "the reason names the deadline that fired: {}",
        envelope["result"]
    );
}

#[test]
fn a_delegate_killed_before_writing_anything_carries_no_partial_key() {
    let envelope = super::timeout_envelope(
        "work",
        super::Expiry::Wall,
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        &super::StreamCapture::default(),
        true,
    );
    assert_eq!(envelope["timed_out"], "wall_clock");
    assert!(envelope.get("partial_result").is_none());
    assert!(envelope.get("session_id").is_none());
}

/// An isolated run without auto-rescue loses its transcript to the runtime
/// teardown. Handing back a session id there would be a handle to nothing, so the
/// envelope says why instead.
#[test]
fn an_unrescuable_killed_run_offers_no_resume_handle() {
    let mut capture = super::StreamCapture::default();
    for line in STREAM.lines().take(4) {
        capture.push_line(line);
    }
    let envelope = super::timeout_envelope(
        "work",
        super::Expiry::Idle,
        Duration::from_secs(400),
        Duration::from_secs(300),
        &capture,
        false,
    );
    assert!(
        envelope.get("session_id").is_none(),
        "no handle for a transcript that is already gone"
    );
    assert_eq!(
        envelope["partial_result"], "alpha",
        "the salvage still comes back"
    );
    let reason = envelope["result"].as_str().expect("reason");
    assert!(
        reason.contains("auto-rescue"),
        "the operator learns about the toggle here, not by losing a second run: {reason}"
    );
}

/// Which runs are resumable at all: the shared tree writes straight into the
/// global store, the isolated one needs the opt-in rescue to survive its own
/// teardown.
#[test]
fn only_a_shared_or_rescued_runtime_leaves_a_transcript_behind() {
    assert!(super::transcript_survives(Isolation::Shared, false));
    assert!(super::transcript_survives(Isolation::Shared, true));
    assert!(!super::transcript_survives(Isolation::Isolated, false));
    assert!(super::transcript_survives(Isolation::Isolated, true));
}

/// Claude Code finds a session only under its own workspace, so a `cwd` that
/// disagrees is refused by name instead of spawning where the transcript is
/// invisible. Both sides canonicalize: one spelling of a path is still that path,
/// which is not academic on macOS, where a tempdir's `/var` is a symlink to
/// `/private/var`.
#[test]
fn a_resume_refuses_a_cwd_that_is_not_the_recorded_workspace() {
    let home = HomeSandbox::new();
    let workspace = home.home().join("ws");
    let elsewhere = home.home().join("other");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&elsewhere).expect("other dir");

    super::check_resume_cwd(workspace.to_str().expect("utf8"), &workspace)
        .expect("the recorded workspace agrees with itself");
    let respelled = workspace.join("..").join("ws");
    super::check_resume_cwd(respelled.to_str().expect("utf8"), &workspace)
        .expect("another spelling of one directory is not another directory");

    let err = super::check_resume_cwd(elsewhere.to_str().expect("utf8"), &workspace)
        .expect_err("a different directory is refused");
    assert!(
        err.contains("not the workspace"),
        "the refusal names what went wrong: {err}"
    );
}

#[test]
fn a_resume_refuses_the_cli_only_latest_shorthand() {
    let err = super::resolve_resume_workspace("latest").expect_err("latest is refused");
    assert!(
        err.contains("exact session id"),
        "a delegate resuming whatever ran last would spend a window on an unrelated session: {err}"
    );
}

/// The stream is the only source for these two on a run that never reached its
/// terminal envelope.
#[test]
fn the_capture_keeps_the_session_id_and_the_newest_throttle_line() {
    let mut capture = super::StreamCapture::default();
    capture.push_line(r#"{"type":"system","subtype":"init","session_id":"s1"}"#);
    capture.push_line(
        r#"{"type":"rate_limit_event","session_id":"s1","rate_limit_info":{"status":"allowed"}}"#,
    );
    capture.push_line(
        r#"{"type":"rate_limit_event","session_id":"s1","rate_limit_info":{"status":"rejected"}}"#,
    );
    assert_eq!(capture.session_id.as_deref(), Some("s1"));
    let throttle = capture.rate_limit_line.as_deref().expect("throttle line");
    assert!(
        throttle.contains("rejected"),
        "the newest throttle line is the one that describes now: {throttle}"
    );
    assert!(
        !capture.envelope_src().contains("rate_limit_event"),
        "a throttle line is not a terminal envelope"
    );
}

/// Salvage is bounded, and the tail is what's kept: the newest text is the part
/// closest to a usable answer.
#[test]
fn salvaged_text_keeps_its_tail_on_a_char_boundary() {
    let mut s = "ä".repeat(40); // 80 bytes, 2 per char
    super::keep_tail(&mut s, 15);
    assert_eq!(s.chars().count(), 7, "clipped to whole chars under the cap");
    assert!(s.chars().all(|c| c == 'ä'));
}

/// The reader is the liveness source: every line it consumes stamps the progress
/// clock the wait loop reads.
#[test]
fn the_stdout_reader_stamps_progress_and_keeps_the_result() {
    let progress = super::AtomicU64::new(u64::MAX);
    let capture = super::read_stdout(
        std::io::Cursor::new(STREAM.as_bytes()),
        true,
        std::time::Instant::now(),
        &progress,
        None,
    );
    assert_ne!(
        progress.load(super::Ordering::Relaxed),
        u64::MAX,
        "each event resets the idle clock"
    );
    assert!(capture.envelope_src().contains("final report"));
    assert_eq!(capture.partial_text(), "alpha");
}

/// A reader that hands back one line per `read` call, sleeping the matching gap
/// first, so a throttle test drives a real clock instead of a fake one.
struct PacedReader {
    lines: std::collections::VecDeque<(std::time::Duration, &'static str)>,
}

impl std::io::Read for PacedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some((gap, line)) = self.lines.pop_front() else {
            return Ok(0);
        };
        std::thread::sleep(gap);
        let bytes = line.as_bytes();
        assert!(bytes.len() <= buf.len(), "fixture line fits one read");
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }
}

/// The heartbeat throttle lives in `read_stdout` so it is testable in one place
/// and the sink stays a pure "write this now" callback. Every job write is an
/// atomic tmp+rename and token deltas arrive at tens per second, so a burst
/// inside one interval must cost exactly one write.
#[test]
fn the_stdout_reader_throttles_the_heartbeat_sink() {
    let delta = |text: &str| {
        format!(
            r#"{{"type":"stream_event","event":{{"delta":{{"type":"text_delta","text":"{text}"}}}}}}"#
        )
    };
    // Leaked so the fixture lines outlive the reader without a lifetime param;
    // four in one burst, then one past the interval.
    let burst: &'static str = Box::leak(format!("{}\n", delta("a")).into_boxed_str());
    let late: &'static str = Box::leak(format!("{}\n", delta("b")).into_boxed_str());
    let zero = std::time::Duration::ZERO;
    let past = super::HEARTBEAT_INTERVAL + std::time::Duration::from_millis(100);
    let reader = PacedReader {
        lines: [
            (zero, burst),
            (zero, burst),
            (zero, burst),
            (zero, burst),
            (past, late),
        ]
        .into_iter()
        .collect(),
    };

    let progress = super::AtomicU64::new(0);
    let mut beats: Vec<String> = Vec::new();
    let mut sink = |capture: &super::StreamCapture| {
        beats.push(super::tail_line(capture));
    };
    super::read_stdout(
        reader,
        true,
        std::time::Instant::now(),
        &progress,
        Some(&mut sink),
    );

    assert_eq!(
        beats.len(),
        2,
        "four lines inside one interval cost one write, the line past it a second: {beats:?}",
    );
    assert_eq!(beats[0], "a", "the first line beats immediately");
    assert_eq!(
        beats[1], "aaaab",
        "the second beat carries everything read since"
    );
}

/// A blocking `delegate` has no job file, so it passes no sink at all: that
/// keeps `read_stdout` pure under test and makes "heartbeats are
/// background-only" structural rather than a runtime branch.
#[test]
fn a_sinkless_reader_never_beats() {
    let progress = super::AtomicU64::new(0);
    let capture = super::read_stdout(
        std::io::Cursor::new(STREAM.as_bytes()),
        true,
        std::time::Instant::now(),
        &progress,
        None,
    );
    assert_eq!(capture.partial_text(), "alpha");
}

/// The tail rides a reply a model may fetch repeatedly, so it is bounded far
/// under the 8 KiB salvage that rides one terminal envelope, and it is one line:
/// a delegate's answer is full of newlines, and a running status is a status.
#[test]
fn tail_line_collapses_whitespace_and_bounds_its_length() {
    let capture = super::StreamCapture {
        text: "  clippy   clean,\n0 warnings.\n\n  moving on \t".to_string(),
        ..Default::default()
    };
    assert_eq!(
        super::tail_line(&capture),
        "clippy clean, 0 warnings. moving on",
        "every whitespace run collapses to one space, and the ends are trimmed",
    );

    // A multi-byte char straddling the cap must not be split. The cap is even,
    // so a 3-byte char is what actually lands the clip mid-codepoint and makes
    // `keep_tail` walk forward to the next boundary.
    // 3 bytes per char.
    let capture = super::StreamCapture {
        text: "€".repeat(super::TAIL_CAP),
        ..Default::default()
    };
    let line = super::tail_line(&capture);
    assert!(
        line.len() <= super::TAIL_CAP,
        "bounded: {} bytes",
        line.len()
    );
    assert!(
        line.chars().all(|c| c == '€'),
        "no replacement char crept in"
    );
    assert_eq!(
        line.chars().count(),
        super::TAIL_CAP / 3,
        "clipped to whole chars under the cap"
    );
}

/// The 3600 s cap rests on progress notifications re-anchoring Claude Code's
/// 30-minute stdio idle abort. A peer that sent no `progressToken` cannot
/// receive them, so the unclamped cap would turn every long wait into a hard
/// abort. The token IS the capability probe, and the clamp is a pure function
/// because the sink itself is not constructible in a test.
#[test]
fn the_wait_cap_clamps_without_a_progress_token() {
    assert_eq!(
        clamp_wait(Some(3600), true),
        3600,
        "the full cap with progress"
    );
    assert_eq!(
        clamp_wait(Some(9999), true),
        MAX_WAIT_SECS,
        "clamped to the cap"
    );
    assert_eq!(
        clamp_wait(Some(3600), false),
        MAX_WAIT_SECS_NO_PROGRESS,
        "a peer that cannot receive progress waits under its idle abort",
    );
    const {
        assert!(
            MAX_WAIT_SECS_NO_PROGRESS < 1800,
            "the clamp must sit under Claude Code's 30-minute stdio idle abort",
        );
    }
    assert_eq!(
        clamp_wait(None, true),
        0,
        "an unset wait still replies instantly"
    );
    assert_eq!(clamp_wait(Some(30), false), 30, "a short wait is untouched");
}

/// `unknown job_id` conflated five causes and named none. Each branch says what
/// the caller can do about it; (2) already collected and (4) dropped past the
/// retention cap leave nothing on disk to tell them apart, so they share a
/// branch rather than inventing a distinction.
#[test]
fn an_unknown_job_id_names_which_cause_it_was() {
    let now = 4_000_000_000_000u64;

    let never = unknown_job_reason("not-a-job", now);
    assert!(
        never.contains("never minted") && never.contains("d-<epoch_ms>-<counter>"),
        "an id off the mint shape says so and names the shape: {never}",
    );

    let swept = unknown_job_reason(&format!("d-{}-0", now - 2 * 60 * 60 * 1000), now);
    assert!(
        swept.contains("swept") && swept.contains("re-run"),
        "an id older than the done TTL names the sweep and the fix: {swept}",
    );
    // Collection leads even on the aged branch: every collect evicts, while the
    // hour-after-finish sweep runs at startup alone, so on a session that has
    // been up a while the sweep is the rarer cause of the two.
    assert!(
        swept.find("already collected") < swept.find("swept"),
        "the likelier cause leads: {swept}",
    );

    let collected = unknown_job_reason(&format!("d-{}-3", now - 1000), now);
    assert!(
        collected.contains("already collected") && collected.contains("256"),
        "a fresh id names collection and the retention cap together: {collected}",
    );
    for reason in [&never, &swept, &collected] {
        assert!(
            reason.starts_with("unknown job_id: "),
            "every cause keeps the lead the caller greps for: {reason}"
        );
    }
}

/// A caller-pinned format is read whole: it is one JSON document, and splitting
/// it on lines would break a pretty-printed one.
#[test]
fn a_pinned_output_format_is_captured_as_one_document() {
    let raw = "{\n  \"type\": \"result\",\n  \"result\": \"pinned\"\n}\n";
    let progress = super::AtomicU64::new(0);
    let capture = super::read_stdout(
        std::io::Cursor::new(raw.as_bytes()),
        false,
        std::time::Instant::now(),
        &progress,
        None,
    );
    let envelope = super::parse_delegate_envelope(capture.envelope_src().trim())
        .expect("whole document parses");
    assert_eq!(envelope["result"], "pinned");
}

// ── bare-session marker gate ─────────────────────────────────────────────────

/// A `clauth mcp` reading the GLOBAL credentials is the MCP half of a bare
/// `claude`; every isolated tier reads its own `.credentials.json` — a supervised
/// `clauth start` session (already registered) or a `delegate` child (which gets
/// `CLAUDE_CONFIG_DIR` in the same builder as its depth marker).
#[test]
fn only_a_globally_authenticated_server_registers_a_bare_marker() {
    use crate::which::SessionAuth;

    assert!(bare_marker_wanted(&SessionAuth::Global, false));
    assert!(!bare_marker_wanted(
        &SessionAuth::IsolatedRuntime("work".to_string()),
        false
    ));
    assert!(!bare_marker_wanted(&SessionAuth::IsolatedCustom, false));
}

/// The Plugin tab's `r` handshake boots a real `clauth mcp` child. Without the
/// marker its 3s life would land on the tally as a session nobody is running —
/// and the probe inherits no `CLAUDE_CONFIG_DIR` of its own to be caught by.
#[test]
fn the_plugin_probes_own_child_registers_no_bare_marker() {
    assert!(!bare_marker_wanted(
        &crate::which::SessionAuth::Global,
        true
    ));
}

/// The load-bearing warnings `delegate`'s description must keep, per the
/// slice-1 replacement text (plan §5's kept list: what a delegate is, that it
/// spends, blindness to this conversation, the four cost shapes, the
/// `background` steer, the `isolated` choice, the `cwd` footgun, spot
/// verification). A `#[tool(description = ...)]` attribute has no other test
/// reaching it, so dropping one during a prose edit would otherwise be silent.
#[test]
fn the_delegate_description_keeps_its_load_bearing_warnings() {
    let tools = ClauthServer::new().tool_router.list_all();
    let delegate = tools
        .iter()
        .find(|t| t.name == "delegate")
        .expect("delegate tool is registered");
    let text = delegate.description.as_deref().unwrap_or_default();

    for phrase in [
        // spends a real account's window or money
        "spends that account's window or money",
        // a delegate is blind to this conversation, so the prompt is the whole brief
        "nothing of this conversation",
        // the four cost shapes, each named
        "burns that subscription's 5h window",
        "bills real money",
        "draws down a prepaid plan",
        "host is free",
        // the background steer
        "prefer it for a slow or third-party target",
        // the isolated-versus-repo-tools choice
        "Leave it false when the task needs this repo's tools",
        // the cwd footgun
        "point `cwd` at a clean dir",
        // a self-report is not a verified result
        "spot-verify its `result` like any subagent",
    ] {
        assert!(
            text.contains(phrase),
            "`delegate` description dropped {phrase:?}: {text}",
        );
    }
}

/// `which` folded into `profiles({scope: "session"})`, and the old
/// `source`-value enumeration went with it: the reply carries `source` in
/// plain text, so pre-teaching four variant names buys nothing. What the
/// description must still name is the scope that reaches it.
#[test]
fn the_profiles_description_names_the_session_scope() {
    let tools = ClauthServer::new().tool_router.list_all();
    let profiles = tools
        .iter()
        .find(|t| t.name == "profiles")
        .expect("profiles tool is registered");
    let text = profiles.description.as_deref().unwrap_or_default();
    assert!(
        text.contains("scope: \"session\""),
        "`profiles` description must name the session scope: {text}",
    );
}

/// The roster's sort key. `roster_lines` is pinned on the value, so nothing else
/// reaches the code that PRODUCES it: an inverted subtraction here would order
/// every session's roster backwards, most-spent account first, and the render
/// test would stay green.
#[test]
fn roster_rank_reports_free_percent_from_the_best_known_window() {
    use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, write_profile_cache};
    use crate::providers::{ThirdPartyStats, UsageBar};
    use crate::usage::{UsageInfo, UsageWindow};

    let _home = HomeSandbox::new();
    let window = |utilization: f64| UsageWindow {
        utilization,
        resets_at: None,
    };

    // 5h wins when both are cached: it is the pool a delegate competes for.
    write_profile_cache(
        "both",
        USAGE_CACHE_FILE,
        &UsageInfo {
            five_hour: Some(window(70.0)),
            seven_day: Some(window(10.0)),
            ..Default::default()
        },
    );
    assert_eq!(roster_rank("both"), RosterRank::Window(30.0));

    // 7d carries it when the 5h window is absent.
    write_profile_cache(
        "weekly",
        USAGE_CACHE_FILE,
        &UsageInfo {
            seven_day: Some(window(25.0)),
            ..Default::default()
        },
    );
    assert_eq!(roster_rank("weekly"), RosterRank::Window(75.0));

    // A third-party provider has no `windows`, but its own bars carry the same
    // percentages, and 5h still outranks 7d.
    let bar = |label: &str, pct: f64| UsageBar {
        label: label.to_string(),
        pct,
        resets_at: None,
        used: None,
        total: None,
    };
    write_profile_cache(
        "bars",
        THIRD_PARTY_CACHE_FILE,
        &ThirdPartyStats {
            is_available: true,
            rows: Vec::new(),
            bars: vec![bar("7d", 94.0), bar("5h", 8.0)],
            plan: Some("pro".to_string()),
            endpoint: None,
            best_effort: false,
        },
    );
    assert_eq!(roster_rank("bars"), RosterRank::Window(92.0));

    // A balance-only provider ranks on its wallet instead, carrying the currency
    // so `roster_lines` can keep two of them from ever being compared.
    write_profile_cache(
        "balance",
        THIRD_PARTY_CACHE_FILE,
        &ThirdPartyStats {
            is_available: true,
            rows: vec![crate::providers::StatRow {
                label: "total".to_string(),
                value: "1117.10 CNY".to_string(),
                kind: crate::providers::StatRowKind::Body,
            }],
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    assert_eq!(
        roster_rank("balance"),
        RosterRank::Balance {
            currency: "CNY".to_string(),
            amount: 1117.10,
        }
    );
    assert_eq!(roster_rank("never-cached"), RosterRank::Unknown);
}

/// The wallet parse is deliberately strict. It reads a `total` row, and every
/// provider writes something into one — z.ai's counts tokens. Anything that is
/// not exactly one finite amount and one currency code describes no wallet, and
/// a loose parse would mint a rank out of it and order the roster on token counts.
#[test]
fn parse_balance_takes_an_amount_and_a_currency_and_nothing_else() {
    assert_eq!(parse_balance("31.45 USD"), Some(("USD".to_string(), 31.45)));
    assert_eq!(
        parse_balance("1117.65 CNY"),
        Some(("CNY".to_string(), 1117.65))
    );
    for junk in [
        "123.4M  (1.2k calls)", // z.ai's token total
        "balance unavailable",
        "31.45",
        "31.45 USD extra",
        "USD 31.45",
        "31.45 U",
        "31.45 TOOLONG",
        "31.45 US1",
        // A non-finite amount must never rank: it outranks (inf) or sinks
        // below (nan) every real wallet in its currency group.
        "nan USD",
        "inf USD",
        "infinity CNY",
        "-inf USD",
        "",
    ] {
        assert_eq!(parse_balance(junk), None, "must not rank on `{junk}`");
    }
    // Exponent and explicit-sign forms parse as finite numbers, so they stay
    // accepted: they order sanely, and refusing them would silently drop the
    // wallet rank of an unknown provider that spelled its total that way.
    assert_eq!(parse_balance("1e3 USD"), Some(("USD".to_string(), 1000.0)));
    assert_eq!(parse_balance("+1.5 USD"), Some(("USD".to_string(), 1.5)));
}

/// A profile holding two wallets joins exactly one currency group: the first its
/// provider lists. Appearing in both would double a name in the roster, and
/// picking the larger would be the cross-currency compare this whole design
/// refuses to make.
#[test]
fn a_two_wallet_profile_ranks_on_the_first_currency_listed() {
    use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, write_profile_cache};
    use crate::providers::{StatRow, StatRowKind, ThirdPartyStats};

    let _home = HomeSandbox::new();
    let row = |label: &str, value: &str| StatRow {
        label: label.to_string(),
        value: value.to_string(),
        kind: StatRowKind::Body,
    };
    // DeepSeek's real shape for a dual-wallet account: USD block, then CNY.
    write_profile_cache(
        "both-wallets",
        THIRD_PARTY_CACHE_FILE,
        &ThirdPartyStats {
            is_available: true,
            rows: vec![
                row("USD balance", ""),
                row("total", "1.19 USD"),
                row("granted", "0.00 USD"),
                row("CNY balance", ""),
                row("total", "1117.65 CNY"),
            ],
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    assert_eq!(
        roster_rank("both-wallets"),
        RosterRank::Balance {
            currency: "USD".to_string(),
            amount: 1.19,
        },
        "the first `total` row wins, not the larger amount",
    );
}

// ---- slice 5: salvage on every lossy exit, and cancel ----

/// Finding 18: a resumable run that never captured a session id used to append
/// nothing at all, so the envelope read as silence against a description that
/// promises a resume handle. The other two arms both say where they stand.
#[test]
fn a_resumable_run_with_no_session_id_says_why_there_is_no_handle() {
    let envelope = super::timeout_envelope(
        "work",
        super::Expiry::Wall,
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        &super::StreamCapture::default(),
        true,
    );
    assert!(
        envelope.get("session_id").is_none(),
        "there was no id to hand back"
    );
    let reason = envelope["result"].as_str().expect("reason");
    assert!(
        reason.contains("no resume handle"),
        "the silent arm names its own absence: {reason}"
    );
}

/// `cancel` orders a set of jobs, so it is meaningless in the state-waiting
/// mode — refused by name at the boundary, the same shape `return_on`'s
/// cross-mode refusal takes (placement rule 4).
///
/// The parameter also has to be READ at all: rmcp deserializes tool args with a
/// plain `from_value` and no `deny_unknown_fields`, so an unhandled `cancel`
/// the description itself teaches would be dropped and the call answered as a
/// plain check.
#[test]
fn monitor_refuses_cancel_without_job_ids() {
    let _home = HomeSandbox::new();
    let result = call_monitor_args(MonitorArgs {
        job_ids: None,
        wait_secs: None,
        return_on: None,
        cancel: Some(true),
    });
    assert_eq!(result.is_error, Some(true), "a cross-mode call is refused");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("refusal text");
    assert!(
        text.contains("`cancel`") && text.contains("`job_ids`"),
        "the refusal names both halves of the seam: {text}",
    );
}

/// A streaming run that wrote token deltas and nothing else: no `assistant`
/// event completed a block and no terminal `result` line ever landed, so its
/// stdout is unparseable as an envelope. Real events all carry `session_id`,
/// deltas included, which is where the resume handle comes from here.
const DELTA_ONLY_STREAM: &str = concat!(
    r#"{"type":"stream_event","session_id":"s9","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an "}}}"#,
    "\n",
    r#"{"type":"stream_event","session_id":"s9","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}}"#,
    "\n",
);

fn capture_of(stream: &str, lines: usize) -> super::StreamCapture {
    let mut capture = super::StreamCapture::default();
    for line in stream.lines().take(lines) {
        capture.push_line(line);
    }
    capture
}

/// Finding 13, the non-zero-exit half. The account's window is spent whether or
/// not clauth keeps the output, so a crash after six kilobytes of answer must
/// not hand back a bare stderr string with the text and the resume handle
/// sitting in locals two lines away.
///
/// Driven through the classifier rather than a hand-built envelope: the live
/// spawn paths have no unit test by standing decision (no fake `claude` on
/// PATH — a fake binary would assert nothing about the real envelope contract),
/// so a real `ExitStatus` is the only way in. `#[cfg(unix)]` because
/// `ExitStatusExt::from_raw` is the only constructor for one, and its wait-status
/// encoding is a unix thing.
#[cfg(unix)]
#[test]
fn a_non_zero_exit_still_hands_back_what_the_run_produced() {
    use std::os::unix::process::ExitStatusExt;

    let capture = capture_of(STREAM, 4);
    let outcome = super::classify_run(
        std::process::ExitStatus::from_raw(1 << 8),
        b"boom: auth failed\n",
        &capture,
        "work",
        true,
    );
    let super::RunOutcome::Exited {
        envelope,
        throttle_scan,
    } = outcome
    else {
        panic!("a non-zero exit classifies as an exit");
    };
    assert_eq!(envelope["is_error"], true);
    assert_eq!(
        envelope["partial_result"], "alpha",
        "the text the run had written survives the crash"
    );
    assert_eq!(
        envelope["session_id"], "s1",
        "the handle that finishes the work without paying for it twice"
    );
    assert!(
        envelope.get("timed_out").is_none(),
        "no deadline fired: {envelope}"
    );
    let reason = envelope["result"].as_str().expect("reason");
    assert!(
        reason.starts_with("claude exited with 1: boom: auth failed"),
        "the existing reason text is kept, with the salvage clauses after it: {reason}"
    );
    assert!(
        reason.contains("`partial_result`") && reason.contains("resume"),
        "the reason names both salvage clauses: {reason}"
    );
    assert!(
        throttle_scan.contains("boom: auth failed"),
        "the rate-limit scan text comes back for the caller to record: {throttle_scan}"
    );
}

/// Finding 13, the unparseable half: a clean exit whose stdout was never an
/// envelope loses exactly as much, and for the same reason. `#[cfg(unix)]` for
/// the same reason as the arm above.
#[cfg(unix)]
#[test]
fn an_unparseable_envelope_still_hands_back_what_the_run_produced() {
    use std::os::unix::process::ExitStatusExt;

    let capture = capture_of(DELTA_ONLY_STREAM, 2);
    assert!(
        super::parse_delegate_envelope(capture.envelope_src().trim()).is_err(),
        "the precondition: this run's stdout is not an envelope"
    );
    let outcome = super::classify_run(
        std::process::ExitStatus::from_raw(0),
        b"",
        &capture,
        "work",
        true,
    );
    let super::RunOutcome::Unparseable(envelope) = outcome else {
        panic!("a clean exit with unreadable output classifies as unparseable");
    };
    assert_eq!(envelope["is_error"], true);
    assert_eq!(envelope["partial_result"], "half an answer");
    assert_eq!(envelope["session_id"], "s9");
    assert!(
        envelope.get("timed_out").is_none(),
        "no deadline fired: {envelope}"
    );
    let reason = envelope["result"].as_str().expect("reason");
    assert!(
        reason.starts_with("failed to parse claude output"),
        "the existing reason text is kept: {reason}"
    );
}

/// Finding 14's wiring, testable without a child process because the whole stop
/// decision is one pure function: a cancel is not a deadline, and it must fire
/// with both deadlines still far away.
#[test]
fn the_stop_decision_reads_the_cancel_flag_beside_both_deadlines() {
    let far = Duration::from_secs(3600);
    let tick = Duration::from_secs(1);
    assert_eq!(
        super::stop_reason(false, tick, tick, far, far, true),
        None,
        "nothing fired, so the run continues"
    );
    assert_eq!(
        super::stop_reason(true, tick, tick, far, far, true),
        Some(super::StopReason::Cancelled),
        "the caller's stop does not wait for a deadline"
    );
    assert_eq!(
        super::stop_reason(false, far, tick, far, far, true),
        Some(super::StopReason::Expired(super::Expiry::Wall)),
        "a deadline still fires on its own"
    );
    // The case the ordering exists for. With the two checks the other way round
    // this tick reports the clock, telling a caller their cancel did nothing.
    assert_eq!(
        super::stop_reason(true, far, tick, far, far, true),
        Some(super::StopReason::Cancelled),
        "a cancel and a deadline landing in the same tick is a cancel"
    );
}

/// The registry is a direct handle rather than a flag in the job file: the
/// detached task runs in this same process. An entry lives exactly as long as
/// the run does, so a stale id can never stop a later job that reuses it.
#[test]
fn the_cancel_registry_holds_a_flag_only_while_the_run_is_registered() {
    let id = "d-777000-0";
    assert!(!super::cancel_job(id), "nothing is registered under it yet");
    let guard = super::CancelGuard::register(id);
    assert!(!guard.is_cancelled(), "registering does not cancel");
    assert!(super::cancel_job(id), "a registered id is held");
    assert!(
        guard.is_cancelled(),
        "the flag the supervision loop reads is the one `cancel_job` set"
    );
    drop(guard);
    assert!(
        !super::cancel_job(id),
        "a deregistered id can no longer be cancelled"
    );
}

/// `cancel: true` marks the named jobs and then runs the ordinary collect, so
/// the caller receives what they produced in the same call. The job here is
/// already finalized: the grace exists for a real run's teardown, and this test
/// is about the cancel rather than the wait.
#[test]
fn cancelling_a_live_job_flips_its_flag_and_the_reply_says_so() {
    let _home = HomeSandbox::new();
    let id = "d-778000-0";
    jobs::write_done(
        id,
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": true, "result": "half an answer"}),
    )
    .unwrap();
    let guard = super::CancelGuard::register(id);

    let result = call_monitor_args(MonitorArgs {
        job_ids: Some(vec![id.to_string()]),
        wait_secs: Some(0),
        return_on: None,
        cancel: Some(true),
    });
    assert!(
        guard.is_cancelled(),
        "the running loop's own flag is what `cancel: true` sets"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("reply text");
    assert!(
        text.contains(&format!("asked `{id}` to stop")),
        "the reply names the job it asked to stop: {text}"
    );
    assert!(
        text.contains("half an answer"),
        "and hands back what that job produced, in the same call: {text}"
    );
}

/// A named id this server holds no run for is NAMED, with its causes hedged the
/// way `unknown_job_reason` hedges its four. Coming back as a plain `running`
/// row reads as "the cancel did nothing", which is the ambiguity this rework
/// exists to kill.
#[test]
fn cancelling_a_job_this_server_does_not_hold_names_it_and_hedges_why() {
    let _home = HomeSandbox::new();
    let id = "d-779000-0";
    jobs::write_done(
        id,
        "work",
        1,
        serde_json::json!({"profile": "work", "is_error": false, "result": "landed first"}),
    )
    .unwrap();

    let result = call_monitor_args(MonitorArgs {
        job_ids: Some(vec![id.to_string()]),
        wait_secs: Some(0),
        return_on: None,
        cancel: Some(true),
    });
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("reply text");
    assert!(text.contains(id), "the id is named: {text}");
    assert!(
        text.contains("already be finishing") && text.contains("earlier server process"),
        "both indistinguishable causes are hedged: {text}"
    );
    assert_eq!(
        result.content.len(),
        1,
        "one content block per reply, cancel report included"
    );
}

/// A cancelled run finalizes like every other outcome. A file left `running`
/// would poll forever and leave `mcp-await-job` blocked on a terminal state that
/// never arrives.
#[test]
fn a_cancelled_run_finalizes_as_a_done_error_rather_than_stranding() {
    let _home = HomeSandbox::new();
    let capture = capture_of(STREAM, 4);
    let envelope = super::cancelled_envelope(
        "work",
        "delegate cancelled after 42s".to_string(),
        Duration::from_secs(42),
        &capture,
        true,
    );
    assert_eq!(envelope["is_error"], true);
    assert_eq!(envelope["cancelled"], true);
    assert!(
        envelope.get("timed_out").is_none(),
        "a cancel is a decision, not a deadline: {envelope}"
    );
    assert_eq!(envelope["partial_result"], "alpha");
    assert_eq!(envelope["session_id"], "s1");

    // The finalize `launch_background_delegate` runs on every outcome.
    let id = "d-780000-0";
    jobs::write_done(id, "work", 1, envelope).unwrap();
    let record = jobs::read(id).expect("the job file is finalized");
    assert_eq!(
        record.state,
        jobs::JobState::Done,
        "never stranded `running`"
    );
    assert_eq!(
        record.envelope.expect("envelope")["is_error"],
        true,
        "and finalized as an error, so a client branching on it reads the stop"
    );
}

// ---- slice 5 fix round ----

/// Structural validation THEN the destructive op, in that order. Partitioning
/// the ids through `cancel_job` first killed the live runs in a list the very
/// next check refuses, which is a spend with no undo made on the way to
/// answering "no".
#[test]
fn a_refused_job_ids_list_cancels_nothing() {
    let _home = HomeSandbox::new();
    let live = "d-781000-0";
    let guard = super::CancelGuard::register(live);
    let mut ids: Vec<String> = (0..256).map(|i| format!("d-{i}-0")).collect();
    ids.push(live.to_string());

    let result = call_monitor_args(MonitorArgs {
        job_ids: Some(ids),
        wait_secs: Some(0),
        return_on: None,
        cancel: Some(true),
    });
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("refusal text"),
        "error: `job_ids` capped at 256 ids; got 257",
        "the refusal is the whole reply: nothing was cancelled to report"
    );
    assert!(
        !guard.is_cancelled(),
        "a list the server refuses must not have stopped a run on its way to being refused"
    );
}

/// An id that could never name a job file is not an unheld job. It used to get
/// the two-cause hedge prepended to its own structural refusal, which reads as
/// though clauth went looking for it.
#[test]
fn cancelling_an_unsafe_job_id_refuses_it_rather_than_hedging_it() {
    let _home = HomeSandbox::new();
    let result = call_monitor_args(MonitorArgs {
        job_ids: Some(vec!["../etc".to_string()]),
        wait_secs: Some(0),
        return_on: None,
        cancel: Some(true),
    });
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("refusal text"),
        "error: invalid job_id",
        "the structural refusal is the whole reply"
    );

    // Several ids do not refuse over one unsafe member — it resolves to
    // `unknown` in its own slot — so the note is what has to leave it alone.
    let mixed = call_monitor_args(MonitorArgs {
        job_ids: Some(vec!["d-1-0".to_string(), "../etc".to_string()]),
        wait_secs: Some(0),
        return_on: None,
        cancel: Some(true),
    });
    let text = mixed
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("reply text");
    let (note, _) = text
        .split_once('\n')
        .expect("the cancel note leads the reply");
    // Identify the line as the note BEFORE reading anything off it. With no
    // note at all the first line is `monitor_batch`'s own `job \`d-1-0\`
    // unknown`, which satisfies both halves below for reasons that have nothing
    // to do with the filter under test.
    assert!(
        note.starts_with("asked ") || note.starts_with("no running delegate here for "),
        "the line under test is the cancel note, not the batch's own first row: {note}"
    );
    assert!(
        note.contains("`d-1-0`") && !note.contains("../etc"),
        "the cancel report covers the ids that could name a job, and no others: {note}"
    );
}

/// The flag is read by the supervision loop, which does not exist until the
/// child is spawned — and `ProfileRuntime::acquire` BLOCKS behind a live
/// `clauth start` session on the same profile. So the reply can only claim the
/// ask, never the outcome; asserting "stopped" put that word directly above a
/// running row for the same id.
#[test]
fn the_cancel_report_claims_the_ask_and_never_the_outcome() {
    let note = super::cancel_note(&["d-1-0".to_string()], &[]);
    assert!(
        note.contains("asked `d-1-0` to stop"),
        "the observable is the ask: {note}"
    );
    assert!(
        !note.contains("stopped"),
        "nothing here has read the flag yet: {note}"
    );
}

/// Registration rides the MINT, not the spawn. The id is in the caller's hands
/// the moment `reserve_background_job` returns, and a cancel landing before the
/// blocking pool picks the task up used to be a silent no-op — hedged under two
/// causes that were both false — leaving the run to its full `timeout_secs`,
/// which is finding 14's original harm.
#[test]
fn a_reserved_job_is_cancellable_before_its_task_starts() {
    let _home = HomeSandbox::new();
    let reserved = reserve_background_job("work", None, None, true).expect("reserve");
    let job_id = reserved.spec.job_id.clone();
    assert!(
        super::cancel_job(&job_id),
        "the reserve registers, so the id the caller holds is already cancellable"
    );
    drop(reserved);
    assert!(
        !super::cancel_job(&job_id),
        "and the entry goes with the reservation"
    );
}

/// `(None, false)` used to land in the isolated-transcript arm and tell a run
/// that never had a session its transcript was lost to auto-rescue — two things
/// clauth did not observe. The session question is answered FIRST: no id means
/// no handle whatever the isolation, and the transcript clause only means
/// something when an id exists.
#[test]
fn a_run_with_no_session_never_claims_a_lost_transcript() {
    let envelope = super::salvage_envelope(
        "work",
        "claude exited with 1: boom".to_string(),
        &super::StreamCapture::default(),
        false,
    );
    let reason = envelope["result"].as_str().expect("reason");
    assert!(
        !reason.contains("auto-rescue") && !reason.contains("isolated runtime"),
        "clauth saw no transcript and no session, so it asserts neither: {reason}"
    );
    assert!(
        reason.contains("no resume handle"),
        "it answers the question the caller actually has: {reason}"
    );
}

/// Where the pre-spawn check sits, which is the half its behavioural twin
/// (`a_run_cancelled_before_it_spawns_says_the_window_was_not_spent`) cannot
/// see: that one proves the arm returns the right envelope, and infers "no
/// child" only from a reason string the other arm happens not to use.
///
/// It observes the source text and nothing else — it cannot tell whether the
/// expression is ever true, which is exactly why the behavioural test carries
/// the weight. So it pins the guard WHOLE rather than merely mentioning it:
/// `if false &&`, a negation, or a swap for a constant all keep the literal
/// `CancelGuard::is_cancelled` and would slip past a `contains` of it. This
/// repo has already paid for that lesson once, on the reader-window guard
/// below, which missed `?` and `return;` until it was widened.
#[test]
fn run_delegate_reads_the_cancel_flag_between_the_acquire_and_the_spawn() {
    let src = include_str!("../../src/mcp/mod.rs");
    let body = src
        .split_once("fn run_delegate(")
        .expect("run_delegate is defined")
        .1;
    let acquire = body
        .find("let runtime = ProfileRuntime::acquire(")
        .expect("the runtime is acquired");
    let spawn = body
        .find("let mut child = command")
        .expect("the child is spawned");
    assert!(acquire < spawn, "the spawn follows the acquire");
    let window = &body[acquire..spawn];
    assert!(
        window.contains("\n    if cancel.is_some_and(CancelGuard::is_cancelled) {\n"),
        "the cancel guard between the acquire and the spawn must be the whole \
         condition, unqualified: {window}"
    );
}

/// The grace is a floor on the wait, not a replacement for it, and it never
/// lifts the ceiling a peer without progress notifications has to live under.
#[test]
fn the_effective_wait_floors_a_cancel_at_the_grace_without_lifting_the_ceiling() {
    assert_eq!(
        super::effective_wait(Some(0), true, false),
        0,
        "an ordinary check still replies instantly"
    );
    assert_eq!(
        super::effective_wait(Some(0), true, true),
        super::CANCEL_GRACE_SECS,
        "a bare cancel waits the grace out instead of replying before anything can move"
    );
    assert_eq!(
        super::effective_wait(Some(600), true, true),
        600,
        "a floor, never a replacement"
    );
    assert_eq!(
        super::effective_wait(Some(9999), false, true),
        1500,
        "and the no-progress ceiling still binds"
    );
}

/// You asked to stop all of them, so you hear about all of them — otherwise the
/// first lane to land ends the wait and the rest come back as `running` rows
/// under a reply that just cancelled them.
#[test]
fn a_cancel_hears_about_every_job_it_named() {
    assert_eq!(
        super::resolve_return_on(None, true, true),
        Ok(super::ReturnOn::All),
        "cancelling with nothing named waits for all of them"
    );
    assert_eq!(
        super::resolve_return_on(None, true, false),
        Ok(super::ReturnOn::Any),
        "an ordinary collect still returns on the first lane"
    );
    assert_eq!(
        super::resolve_return_on(Some("any"), true, true),
        Ok(super::ReturnOn::Any),
        "an explicit value still wins"
    );
}

/// What the extraction MOVED: a throttle hint can hide in the child's stderr,
/// in its stdout, or in a `rate_limit_event` line that never reached either, so
/// all three reach the scan.
///
/// The `record_rate_limit` call the scan feeds is NOT pinned here. It writes to
/// the throughput store as a side effect of a real spawn, and this crate does
/// not fake a `claude` on PATH; keeping the composition pure is what let the
/// half that can be checked be checked.
#[cfg(unix)]
#[test]
fn the_throttle_scan_carries_every_source_a_rate_limit_hides_in() {
    use std::os::unix::process::ExitStatusExt;

    let mut capture = super::StreamCapture::default();
    capture.push_line(r#"{"type":"system","subtype":"init","session_id":"stdout-marker"}"#);
    capture.push_line(r#"{"type":"rate_limit_event","note":"ratelimit-marker"}"#);
    let outcome = super::classify_run(
        std::process::ExitStatus::from_raw(1 << 8),
        b"stderr-marker",
        &capture,
        "work",
        true,
    );
    let super::RunOutcome::Exited { throttle_scan, .. } = outcome else {
        panic!("a non-zero exit classifies as an exit");
    };
    for marker in ["stderr-marker", "stdout-marker", "ratelimit-marker"] {
        assert!(
            throttle_scan.contains(marker),
            "{marker} must reach the scan: {throttle_scan}"
        );
    }
}

/// The pre-spawn arm, driven for real. A cancel that lands while
/// `ProfileRuntime::acquire` blocks must end the run THERE, and the standing
/// no-fake-`claude` decision does not reach this arm: its whole point is that it
/// returns before any child exists, so there is nothing to fake.
///
/// It is the one arm where a source-scan pin would be the weaker check, and the
/// only `run_delegate` test in this file that drives the real acquire — every
/// other one is refused at or before the cwd check.
#[test]
fn a_run_cancelled_before_it_spawns_says_the_window_was_not_spent() {
    let home = HomeSandbox::new();
    // The acquire refuses outright without it ("~/.claude not found; install
    // Claude Code first"), which would pass this test's `Ok` check for the
    // wrong reason if it were ever relaxed to one.
    std::fs::create_dir_all(home.home().join(".claude")).expect("stage ~/.claude");
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "work".to_string(), None, None, None)
        .expect("create profile");

    let job_id = "d-782000-0";
    let guard = super::CancelGuard::register(job_id);
    assert!(
        super::cancel_job(job_id),
        "fixture control: the run is held"
    );

    let envelope = run_delegate(DelegateOpts {
        profile: "work",
        prompt: "hello",
        model: None,
        cwd: None,
        env: HashMap::new(),
        extra_args: Vec::new(),
        timeout_secs: Some(30),
        idle_secs: None,
        resume: None,
        isolation: Isolation::Isolated,
        depth: 0,
        job: None,
        cancel: Some(&guard),
    })
    .expect("a cancel is an envelope, never a transport error");

    assert_eq!(envelope["cancelled"], true);
    assert_eq!(envelope["is_error"], true);
    assert!(
        envelope.get("timed_out").is_none(),
        "a cancel is not a deadline: {envelope}"
    );
    let reason = envelope["result"].as_str().expect("reason");
    assert!(
        reason.contains("before it spawned") && reason.contains("was not spent"),
        "the fact the caller acts on is that nothing was billed: {reason}"
    );
    assert!(
        envelope.get("partial_result").is_none() && envelope.get("session_id").is_none(),
        "no child ran, so there is nothing to salvage: {envelope}"
    );
    // Belt and braces on "no child": a spawned delegate stamps its transcripts
    // into the runtime's own `projects/` on the way out, and this run must not
    // have reached that teardown either.
    assert!(
        !home
            .home()
            .join(".clauth")
            .join("profiles")
            .join("work")
            .join("runtime-isolated")
            .exists(),
        "the isolated runtime is torn down with the guard, not left behind"
    );
}
