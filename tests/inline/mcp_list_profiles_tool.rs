#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Guard coverage for the MCP `list_profiles` tool's response shape.
//!
//! It is the largest single thing clauth puts in front of a model — 3,854 real
//! tokens across one operator's 27 profiles before this trim, against 955 for
//! the whole init block — and its own description tells the model to call it at
//! session start. So the two things keeping it small are worth pinning: the
//! `names` filter, and the fields that appear only when they carry news.

use super::*;

use crate::profile::{AppState, Profile, save_app_state, save_profile};
use crate::testutil::HomeSandbox;

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

/// Drive the async tool on a current-thread runtime, mirroring `call_which`.
fn call_list(format: &str, names: Option<Vec<&str>>) -> CallToolResult {
    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .list_profiles(Parameters(ListProfilesArgs {
                names: names.map(|v| v.into_iter().map(str::to_string).collect()),
                format: Some(format.to_string()),
            }))
            .await
    })
    .expect("list_profiles returns a tool result, never a transport error")
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("first content block is text")
}

fn payload(result: &CallToolResult) -> serde_json::Value {
    serde_json::from_str(&first_text(result)).expect("parse payload")
}

fn names_in(payload: &serde_json::Value) -> Vec<String> {
    payload["profiles"]
        .as_array()
        .expect("profiles array")
        .iter()
        .map(|p| p["name"].as_str().expect("name").to_string())
        .collect()
}

#[test]
fn names_filter_selects_one_profile_case_insensitively() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    assert_eq!(
        names_in(&payload(&call_list("json", None))),
        vec!["solo", "vendor"],
        "fixture control: both profiles are visible unfiltered",
    );
    // Wrong case on purpose: the filter resolves through `canonical_name`, the
    // same guard `switch` applies, so a model need not know the stored casing.
    assert_eq!(
        names_in(&payload(&call_list("json", Some(vec!["VENDOR"])))),
        vec!["vendor"],
    );
    // An empty list is the same ask as no list at all, never "nothing".
    assert_eq!(
        names_in(&payload(&call_list("json", Some(Vec::new())))),
        vec!["solo", "vendor"],
    );
}

/// A name matching nothing fails loudly. Dropping it silently would answer with
/// a roster that reads exactly like "that profile no longer exists", and the
/// model would act on the wrong one of those two readings.
#[test]
fn an_unresolvable_name_is_refused_and_named() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    let result = call_list("json", Some(vec!["solo", "ghost"]));
    assert_eq!(result.is_error, Some(true));

    let body = payload(&result);
    assert_eq!(body["ok"], serde_json::Value::Bool(false));
    let reason = body["reason"].as_str().expect("reason");
    assert!(reason.contains("ghost"), "the reason names the bad input");
    assert!(!reason.contains("solo"), "and only the bad input: {reason}");
    assert!(reason.contains("names"), "and the fix: {reason}");
}

/// The trim itself. `has_live_session` and `throughput` are absent unless they
/// say something, and the endpoint prints as a host. Emitted unconditionally
/// these were 39% of a 27-profile response, nearly all of it `false` and rows
/// carrying no warning at all.
#[test]
fn quiet_fields_are_absent_and_the_endpoint_prints_as_a_host() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    let body = payload(&call_list("json", None));
    let rows = body["profiles"].as_array().expect("profiles array");
    let solo = &rows[0];
    let vendor = &rows[1];

    for row in rows {
        let name = &row["name"];
        assert!(
            row.get("has_live_session").is_none(),
            "{name} has no live session, so the field must not appear",
        );
        assert!(
            row.get("throughput").is_none(),
            "{name} has no degraded model, so the field must not appear",
        );
        assert!(
            row.get("base_url").is_none(),
            "{name} must carry the host, never the full endpoint",
        );
    }

    // Host only: every profile of one provider repeats the same path, and the
    // cost model only ever asks whether the host is loopback or LAN.
    assert_eq!(vendor["host"], "api.deepseek.com");
    assert!(
        solo.get("host").is_none(),
        "a default OAuth profile has no endpoint at all",
    );

    // The fields a picker always needs stay unconditional, `null` included, so
    // their absence never has to be guessed at.
    for key in [
        "name",
        "active",
        "provider",
        "tier",
        "windows",
        "third_party",
    ] {
        assert!(solo.get(key).is_some(), "`{key}` must always be present");
    }
    assert_eq!(solo["active"], serde_json::Value::Bool(true));
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

fn row_named<'a>(payload: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    payload["profiles"]
        .as_array()
        .expect("profiles array")
        .iter()
        .find(|p| p["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("no profile named {name}"))
}

/// `keyless` is the only-when-news signal a picker needs before a `delegate`:
/// true on a third-party profile with no inference auth source, absent (never
/// `false`) on a keyed third-party and on an OAuth profile.
#[test]
fn keyless_flag_names_only_the_keyless_third_party_profile() {
    let _home = HomeSandbox::new();
    seed_auth_states();

    let body = payload(&call_list("json", None));

    assert_eq!(
        row_named(&body, "keyless")["keyless"],
        serde_json::Value::Bool(true),
        "a third-party profile with no inference auth source carries the flag",
    );
    assert!(
        row_named(&body, "keyed").get("keyless").is_none(),
        "a keyed third-party profile must not carry `keyless` at all",
    );
    assert!(
        row_named(&body, "solo").get("keyless").is_none(),
        "an OAuth profile has no provider, so `keyless` never applies",
    );
}

/// The prose names the keyless profile in words and leaves the keyed and OAuth
/// lines exactly as they rendered before the field existed.
#[test]
fn prose_names_the_keyless_profile_and_leaves_the_others_unchanged() {
    let _home = HomeSandbox::new();
    seed_auth_states();

    let text = first_text(&call_list("prose", None));
    let mut lines = text.lines();

    assert_eq!(
        lines.next(),
        Some("- solo (active) [anthropic]: usage unknown; tier unknown"),
        "the OAuth line renders as before",
    );
    assert_eq!(
        lines.next(),
        Some("- keyed [DeepSeek, api.deepseek.com]: usage unknown; balance unknown"),
        "the keyed third-party line renders as before, no keyless clause",
    );
    assert_eq!(
        lines.next(),
        Some("- keyless [DeepSeek, api.deepseek.com]: usage unknown; balance unknown; no api key"),
        "the keyless profile names its missing api key in words",
    );
    assert_eq!(lines.next(), None, "exactly three lines, nothing appended");
}
