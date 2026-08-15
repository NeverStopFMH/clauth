#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Guard coverage for the MCP `profiles` tool's response shape.
//!
//! It is the largest single thing clauth puts in front of a model — 3,854 real
//! tokens across one operator's 27 profiles before this trim, against 955 for
//! the whole init block — and its own description tells the model to call it at
//! session start. So the two things keeping it small are worth pinning: the
//! `names` filter, and the fields that appear only when they carry news.
//!
//! The `scope: "session"` arm is the folded-in former `which` tool: it resolves
//! through the same `which::resolve_active` tiers the session itself resolves
//! by, renders its one row through the roster's own `profile_line`, and carries
//! `source`.

use super::*;

use crate::profile::{
    AppState, ClaudeCredentials, OAuthToken, Profile, save_app_state, save_profile,
};
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::testutil::{ConfigDirSandbox, HomeSandbox};
use crate::usage::{PlanInfo, PlanTier, UsageInfo};

/// Two profiles: one plain OAuth account, one third-party with an endpoint.
fn seed_two_profiles() {
    save_profile(&Profile::new("solo".to_string(), None, None)).expect("save solo");
    save_profile(&Profile::new(
        "vendor".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    ))
    .expect("save vendor");
    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "vendor".into()],
        ..Default::default()
    })
    .expect("save state");
}

/// Drive the async tool on a current-thread runtime.
fn call_profiles(names: Option<Vec<&str>>, scope: Option<&str>) -> CallToolResult {
    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .profiles(Parameters(ProfilesArgs {
                names: names.map(|v| v.into_iter().map(str::to_string).collect()),
                scope: scope.map(str::to_string),
            }))
            .await
    })
    .expect("profiles returns a tool result, never a transport error")
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("first content block is text")
}

/// The reply's prose lines, one per roster row.
fn lines(result: &CallToolResult) -> Vec<String> {
    first_text(result).lines().map(str::to_string).collect()
}

#[test]
fn names_filter_selects_one_profile_case_insensitively() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    assert_eq!(
        lines(&call_profiles(None, None)),
        vec![
            "- solo (active) [anthropic]: usage unknown; tier unknown",
            "- vendor [DeepSeek, api.deepseek.com]: usage unknown; balance unknown; no api key",
        ],
        "fixture control: both profiles are visible unfiltered",
    );
    // Wrong case on purpose: the filter resolves through `canonical_name`, the
    // same guard `switch_profile` applies, so a model need not know the stored
    // casing.
    assert_eq!(
        lines(&call_profiles(Some(vec!["VENDOR"]), None)),
        vec!["- vendor [DeepSeek, api.deepseek.com]: usage unknown; balance unknown; no api key"],
    );
    // An empty list is the same ask as no list at all, never "nothing".
    assert_eq!(
        lines(&call_profiles(Some(Vec::new()), None)).len(),
        2,
        "an empty `names` list still answers with every profile",
    );
}

/// A name matching nothing fails loudly. Dropping it silently would answer with
/// a roster that reads exactly like "that profile no longer exists", and the
/// model would act on the wrong one of those two readings.
#[test]
fn an_unresolvable_name_is_refused_and_named() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    let result = call_profiles(Some(vec!["solo", "ghost"]), None);
    assert_eq!(result.is_error, Some(true));
    let text = first_text(&result);
    assert!(
        text.starts_with("error: "),
        "a refusal reads as one: {text}"
    );
    assert!(text.contains("ghost"), "the reason names the bad input");
    assert!(!text.contains("solo"), "and only the bad input: {text}");
    assert!(text.contains("names"), "and the fix: {text}");
}

/// A scope that is neither `all` nor `session` is refused by name: a typo must
/// not silently answer the wrong question, which for two scopes is half of
/// them.
#[test]
fn an_unrecognised_scope_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_profiles(None, Some("sessions"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        first_text(&result),
        "error: unrecognized scope \"sessions\": accepted \"all\" and \"session\""
    );
}

/// The trim itself. `has_live_session` and `throughput` are absent unless they
/// say something, and the endpoint prints as a host. Emitted unconditionally
/// these were 39% of a 27-profile response, nearly all of it `false` and rows
/// carrying no warning at all.
#[test]
fn quiet_fields_are_absent_and_the_endpoint_prints_as_a_host() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    let text = first_text(&call_profiles(None, None));
    let mut rows = text.lines();

    let solo = rows.next().expect("solo line");
    assert!(solo.starts_with("- solo (active) [anthropic]"), "{solo}");
    assert!(
        !solo.contains("live session"),
        "no live session, so the flag must not appear",
    );
    assert!(
        !solo.contains("throughput"),
        "no degraded model, so the field must not appear",
    );
    assert!(
        !solo.contains("https://"),
        "the endpoint must print as a host, never in full",
    );

    let vendor = rows.next().expect("vendor line");
    // Host only: every profile of one provider repeats the same path, and the
    // cost model only ever asks whether the host is loopback or LAN.
    assert!(
        vendor.contains("[DeepSeek, api.deepseek.com]"),
        "the bracket carries the host: {vendor}",
    );

    // The fields a picker always needs stay spelled, `unknown` included, so
    // their absence never has to be guessed at.
    assert!(
        solo.contains("usage unknown") && solo.contains("tier unknown"),
        "a null window and a null tier read as unknown, never drop out: {solo}",
    );
    assert!(solo.contains("(active)"), "the active marker is present");
}

/// Three profiles spanning the auth states: an OAuth account, a keyed
/// third-party, and a keyless third-party. The keyless one is the state the
/// roster's `keyless` flag must separate from "balance not fetched yet".
fn seed_auth_states() {
    save_profile(&Profile::new("solo".to_string(), None, None)).expect("save solo");
    save_profile(&Profile::new(
        "keyed".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-keyed-123".to_string()),
    ))
    .expect("save keyed");
    save_profile(&Profile::new(
        "keyless".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    ))
    .expect("save keyless");
    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "keyed".into(), "keyless".into()],
        ..Default::default()
    })
    .expect("save state");
}

/// `keyless` is the only-when-news signal a picker needs before a `delegate`:
/// true on a third-party profile with no inference auth source, absent (never
/// `false`) on a keyed third-party and on an OAuth profile.
#[test]
fn keyless_flag_names_only_the_keyless_third_party_profile() {
    let _home = HomeSandbox::new();
    seed_auth_states();

    let text = first_text(&call_profiles(None, None));
    assert!(
        !text.lines().next().unwrap().contains("no api key"),
        "an OAuth profile never carries the keyless clause",
    );
    assert!(
        !text.contains("keyed [DeepSeek, api.deepseek.com]: usage unknown; balance unknown; no"),
        "a keyed third-party profile must not carry the keyless clause",
    );
    assert!(
        text.contains(
            "- keyless [DeepSeek, api.deepseek.com]: usage unknown; balance unknown; no api key"
        ),
        "the keyless profile names its missing api key in words: {text}",
    );
}

/// The prose names the keyless profile in words and leaves the keyed and OAuth
/// lines exactly as they rendered before the field existed.
#[test]
fn prose_names_the_keyless_profile_and_leaves_the_others_unchanged() {
    let _home = HomeSandbox::new();
    seed_auth_states();

    let lines = lines(&call_profiles(None, None));

    assert_eq!(
        lines,
        vec![
            "- solo (active) [anthropic]: usage unknown; tier unknown".to_string(),
            "- keyed [DeepSeek, api.deepseek.com]: usage unknown; balance unknown".to_string(),
            "- keyless [DeepSeek, api.deepseek.com]: usage unknown; balance unknown; no api key"
                .to_string(),
        ],
        "three lines: the OAuth and keyed lines render as before, the keyless          one names its missing api key in words",
    );
}

// ── scope: "session" (the folded-in former `which` tool) ─────────────────────

/// Seed one account in the canceled-after-login shape: its stored token still
/// claims `pro` (written once at login, never refreshed) while its cached
/// `/profile` plan has moved to `Free`.
fn seed_canceled_account() {
    let mut profile = Profile::new("kerry".to_string(), None, None);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-kerry".to_string(),
            refresh_token: Some("rt-kerry".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: Some("pro".to_string()),
        }),
    });
    save_profile(&profile).expect("save profile");
    save_app_state(&AppState {
        active_profile: Some("kerry".into()),
        profiles: vec!["kerry".into()],
        ..Default::default()
    })
    .expect("save state");

    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    };
    write_profile_cache("kerry", USAGE_CACHE_FILE, &usage);
}

/// The session row resolves through the same tiers `which` used and reports
/// `source`, on a row rendered by the roster's own `profile_line` — which is
/// what makes the tier read the cached plan, not the login claim
/// (`profile_json::tier_label`, the same helper `clauth which --json` and
/// `status.json` call). A canceled account reports the org's post-cancellation
/// tier, never the one its stored token still claims.
#[test]
fn session_scope_resolves_the_tier_through_the_which_tiers() {
    let home = HomeSandbox::new();
    seed_canceled_account();
    // Resolve by runtime dir rather than by loaded credentials: the `session_dir`
    // tier attributes the session from the path alone, so the fixture does not
    // depend on whatever `~/.claude` holds. The `<pid>-<seq>` shape is load
    // bearing — `is_session_id` rejects anything else and the session would fall
    // through unresolved.
    let runtime = home.home().join(".clauth/profiles/kerry/runtime-4242-1");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let _dir = ConfigDirSandbox::new(&home, &runtime);

    let result = call_profiles(None, Some("session"));
    let text = first_text(&result);
    let row = text.lines().next().expect("the session row");
    assert!(
        row.starts_with("- kerry (active) [anthropic, Free]"),
        "one row, resolved to the seeded account with the CACHED tier: {row}",
    );
    assert!(
        row.contains("source `session_dir`"),
        "the row names how it resolved: {row}",
    );
    // Live usage rides the session reply like it rode `which`'s.
    assert!(
        text.contains("; active profile `kerry`: 5h unknown, 7d unknown"),
        "the active profile's live usage follows the row: {text}",
    );
}

/// The session-scope reply carries the switch-effect note and, for the one
/// tier that earns it, the runtime-paths note — through the same renderers the
/// init block uses, so a client that drops the block still sees them
/// (placement rule 3: one renderer, two carriers).
#[test]
fn session_scope_reply_carries_the_session_notes() {
    let home = HomeSandbox::new();
    seed_canceled_account();
    let runtime = home.home().join(".clauth/profiles/kerry/runtime-4242-1");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let _dir = ConfigDirSandbox::new(&home, &runtime);

    let text = first_text(&call_profiles(None, Some("session")));
    assert!(
        text.contains("switch_profile & this session:"),
        "the switch-effect note rides the reply: {text}",
    );
    assert!(
        text.contains("pinned to `kerry`"),
        "the note names this session's own profile: {text}",
    );
    assert!(
        text.contains("runtime paths:"),
        "an isolated-runtime session earns the runtime-paths note: {text}",
    );
}

/// `resolve_active` returning nothing is an unresolved session — a genuine
/// unknown, not "no profiles configured" — and the reply says so instead of
/// rendering an empty roster.
#[test]
fn an_unresolved_session_reads_unknown_not_empty() {
    let home = HomeSandbox::new();
    // A config dir that is nobody's runtime and holds no matching credentials:
    // every tier misses.
    let foreign = home.home().join("foreign-config");
    std::fs::create_dir_all(&foreign).expect("foreign dir");
    let _dir = ConfigDirSandbox::new(&home, &foreign);

    let text = first_text(&call_profiles(None, Some("session")));
    let row = text.lines().next().expect("the session line");
    assert!(
        row.starts_with("session profile unknown, source unknown"),
        "an unresolved session is an unknown, never `no profiles`: {row}",
    );
}

/// `names` filters the roster; the session scope IS already a one-row answer,
/// so the pair is a cross-mode mistake. Refused by name with the fix — the
/// same boundary rule as `monitor`'s job/state seam — instead of silently
/// ignoring a name the all-scope arm would have refused.
#[test]
fn session_scope_refuses_names_by_name() {
    let _home = HomeSandbox::new();
    let result = call_profiles(Some(vec!["ghost"]), Some("session"));
    assert_eq!(result.is_error, Some(true), "the combination is refused");
    assert_eq!(
        first_text(&result),
        "error: `names` cannot combine with `scope: \"session\"`: the session scope answers the \
         one account this session runs on; drop `names`",
    );
    // An empty list stays the established "same as omitted" spelling, not a
    // refusal: that is the convention `names` itself documents.
    let empty = call_profiles(Some(Vec::new()), Some("session"));
    assert_ne!(
        empty.is_error,
        Some(true),
        "an empty `names` list is omitted"
    );
}
