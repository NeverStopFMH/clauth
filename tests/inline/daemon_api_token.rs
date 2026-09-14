//! The REST API's bearer token: shape, persistence, and the constant-time
//! check. What these pin is that the credential an operator copies to another
//! machine keeps working across restarts, and that a file which is not a token
//! is never treated as one.
//!
//! All disk state is redirected into a [`HomeSandbox`] tempdir, so nothing here
//! reads or writes the operator's real `~/.clauth/auth_token.json`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use crate::testutil::HomeSandbox;
// The 0600 tree invariant, and so the helper that checks it, is Unix-only —
// `atomic_write_600` falls back to a plain write elsewhere. Both imports serve
// only `token_file_is_owner_only`, which is gated the same way.
#[cfg(unix)]
use crate::profile::clauth_dir;
#[cfg(unix)]
use crate::testutil::owner_only_violations;

#[test]
fn generated_token_is_64_lowercase_hex() {
    let token = generate().expect("generate");
    assert_eq!(token.len(), 64, "a SHA-256 hex digest is 64 chars");
    assert!(is_well_formed(&token), "{token} failed its own shape check");
    assert_eq!(token, token.to_lowercase(), "hex must be lowercase");
}

#[test]
fn two_generations_differ() {
    // Not a randomness test, a wiring test: a constant seed would sail through
    // every other assertion in this file.
    let a = generate().expect("generate");
    let b = generate().expect("generate");
    assert_ne!(a, b);
}

#[test]
fn token_persists_across_calls() {
    let _home = HomeSandbox::new();
    let first = load_or_create().expect("first load");
    let second = load_or_create().expect("second load");
    assert_eq!(
        first, second,
        "a restart must not invalidate the token already copied to a client"
    );
}

/// Unix-only: Windows has no mode bits, and `atomic_write_600` writes plainly
/// there, so there is no invariant left to assert.
#[cfg(unix)]
#[test]
fn token_file_is_owner_only() {
    let _home = HomeSandbox::new();
    load_or_create().expect("load");
    let left = owner_only_violations(&clauth_dir().expect("clauth dir"));
    assert!(
        left.is_empty(),
        "the token file must inherit the 0600 tree invariant; still loose: {left:#?}"
    );
}

#[test]
fn rotate_replaces_the_stored_token() {
    let _home = HomeSandbox::new();
    let original = load_or_create().expect("load");
    let rotated = rotate().expect("rotate");
    assert_ne!(original, rotated);
    assert_eq!(
        load_or_create().expect("reload"),
        rotated,
        "rotation must persist, not just return a new value"
    );
}

/// A file that is not a well-formed token is replaced rather than trusted: half
/// a token is not a weaker token, it is no token.
#[test]
fn malformed_token_files_are_regenerated() {
    for bad in [
        r#"{"schema":1,"token":"short","created_at":"x"}"#,
        r#"{"schema":1,"token":"NOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXZZZZ","created_at":"x"}"#,
        // Upper-case hex: not what `generate` emits, so not what we accept.
        r#"{"schema":1,"token":"AAAABBBBCCCCDDDDEEEEFFFF00001111AAAABBBBCCCCDDDDEEEEFFFF00001111","created_at":"x"}"#,
        "not json at all",
        "",
    ] {
        let _home = HomeSandbox::new();
        let path = token_path().expect("path");
        crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, bad).expect("seed");

        let token = load_or_create().expect("load");
        assert!(
            is_well_formed(&token),
            "{bad:?} should have been replaced, got {token:?}"
        );
    }
}

/// `read_valid`'s schema note latches process-wide. The tests that trip it all
/// hold a [`HomeSandbox`] too, and every sandbox takes `HOME_TEST_LOCK` for its
/// whole life, so the latch observers are already serialized by the sandbox
/// itself (nextest isolates processes; the plain `cargo test` fallback runs
/// tests as threads of one process). Never add a second mutex for this: an
/// unranked test mutex acquired in both orders across tests is an ABBA
/// deadlock under the threaded harness.
#[test]
fn future_schema_token_is_reused_when_well_formed() {
    let _home = HomeSandbox::new();
    let future = "a".repeat(64);
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        format!(r#"{{"schema":99,"token":"{future}","created_at":"x"}}"#),
    )
    .expect("seed");

    assert_eq!(load_or_create().expect("load"), future);
}

/// The schema-too-new note is a process-wide one-shot: `current_or` re-reads the
/// file for every request, and an unlatched note would be a line per request —
/// a tray polling twice a minute writing that line twice a minute for the
/// daemon's life.
#[test]
fn the_schema_note_is_logged_once_not_per_read() {
    let _home = HomeSandbox::new();
    let future = "c".repeat(64);
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        format!(r#"{{"schema":99,"token":"{future}","created_at":"x"}}"#),
    )
    .expect("seed");

    reset_schema_note_for_tests();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    assert_eq!(read_valid().expect("read"), Some(future.clone()));
    assert_eq!(read_valid().expect("read"), Some(future));
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.contains("this build knows"))
            .count(),
        1,
        "once per process, not once per read"
    );
}

// ── tier ────────────────────────────────────────────────────────────────────

/// A freshly minted file says what the token may do.
///
/// Written now so that a later read-only or mirror-only token is a new value in
/// a field every deployed file already carries, rather than a schema bump with a
/// migration behind it.
#[test]
fn a_fresh_token_file_records_the_control_tier() {
    let _home = HomeSandbox::new();
    load_or_create().expect("load");

    let body = std::fs::read_to_string(token_path().expect("path")).expect("read");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["tier"], serde_json::json!("control"));
}

/// A file written before the field existed is reused, not rotated.
///
/// This is the whole upgrade path: every `auth_token.json` in the field today
/// lacks `tier`, and rotating them on upgrade would 401 every client the
/// operator had already set up.
#[test]
fn a_file_without_a_tier_reads_as_control() {
    let _home = HomeSandbox::new();
    let existing = "b".repeat(64);
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        format!(r#"{{"schema":1,"token":"{existing}","created_at":"x"}}"#),
    )
    .expect("seed");

    assert_eq!(
        load_or_create().expect("load"),
        existing,
        "an upgrade must not rotate the token every client already holds"
    );
}

/// A tier this build does not know refuses, and leaves the file alone.
///
/// Both halves matter and they pull in opposite directions. Serving it would
/// promote a token a newer clauth deliberately restricted; replacing it would
/// revoke, from a downgrade, a credential the operator distributed on purpose.
/// So the only safe move is to do neither and say so.
#[test]
fn an_unknown_tier_refuses_rather_than_serving_or_replacing() {
    // Schema 2 > 1 trips the schema note's latch on the way to the tier bail;
    // the sandbox's `HOME_TEST_LOCK` serializes the latch observers (see the
    // `future_schema` test's doc).
    let _home = HomeSandbox::new();
    let restricted = "c".repeat(64);
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    let seeded =
        format!(r#"{{"schema":2,"token":"{restricted}","created_at":"x","tier":"readonly"}}"#);
    std::fs::write(&path, &seeded).expect("seed");

    let err = load_or_create().expect_err("an unknown tier must not be served");
    assert!(
        format!("{err:#}").contains("readonly"),
        "the operator has to be told which tier stopped it: {err:#}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        seeded,
        "the file must survive untouched — replacing it would revoke a token on a downgrade"
    );
}

// ── tier on the live path ────────────────────────────────────────────────────

/// The live-path refusal is TYPE-DISTINGUISHABLE: the route answers
/// `503 token_tier_unknown` for the tier arm and `500 internal` for anything
/// else `current_or` can error with. One blanket arm would label an IO
/// failure with a code that tells the operator to downgrade — wrong advice
/// for a full disk.
#[test]
fn the_route_distinguishes_the_tier_refusal_from_other_read_errors() {
    let _home = HomeSandbox::new();
    let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ));
    let status_path = token_path().expect("path").with_file_name("status.json");
    let ctx = crate::daemon::api::routes::ApiContext::new(
        config,
        status_path,
        AuthToken::from_plaintext(&"e".repeat(64)),
        None,
    );
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    let body = format!(
        r#"{{"schema":1,"token":"{}","created_at":"x","tier":"readonly"}}"#,
        "f".repeat(64)
    );
    std::fs::write(&path, body).expect("seed");

    let resp = crate::daemon::api::routes::handle(
        &ctx,
        &crate::daemon::api::http::Request {
            method: "GET".to_string(),
            path: "/api/v1/health".to_string(),
            query: String::new(),
            bearer: Some("f".repeat(64)),
            if_none_match: None,
            body: Vec::new(),
            keep_alive: true,
        },
    );
    assert_eq!(resp.status, 503);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&resp.body).expect("json")["error"],
        serde_json::json!("token_tier_unknown")
    );
}

/// [`current_or`] is the live read behind every request, and it must refuse an
/// unknown tier exactly as [`load_or_create`] does at startup. The startup
/// refusal pins a file the daemon was never started with; this pins a file a
/// NEWER clauth wrote under a running one — a `--rotate-token` from a build
/// that knows a restricted tier. The old fallback here was the spawn-time
/// token, which is the credential the rotation just replaced: the refused
/// birth of round-1 blocker 3 (`--rotate-token` never revoking against a
/// running daemon) re-arms through a version-skew door.
#[test]
fn the_live_read_refuses_an_unknown_tier_rather_than_keeping_the_spawn_token() {
    let _home = HomeSandbox::new();
    let spawned = AuthToken::from_plaintext(&"e".repeat(64));
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    let body = format!(
        r#"{{"schema":1,"token":"{}","created_at":"x","tier":"readonly"}}"#,
        "f".repeat(64)
    );
    std::fs::write(&path, body).expect("seed");

    let err = current_or(&spawned).expect_err("the live read must refuse");
    assert!(
        format!("{err:#}").contains("readonly"),
        "the operator has to be told which tier stopped it: {err:#}"
    );
}

/// The refusal is one line in the log, not one per request: the read runs for
/// every request, and a tray polling twice a minute would write the line twice
/// a minute for the daemon's life.
#[test]
fn the_live_read_refusal_is_logged_once_not_per_read() {
    let _home = HomeSandbox::new();
    let spawned = AuthToken::from_plaintext(&"e".repeat(64));
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    let body = format!(
        r#"{{"schema":1,"token":"{}","created_at":"x","tier":"readonly"}}"#,
        "f".repeat(64)
    );
    std::fs::write(&path, body).expect("seed");

    reset_schema_note_for_tests();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    assert!(current_or(&spawned).is_err());
    assert!(current_or(&spawned).is_err());
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.contains("this build does not know"))
            .count(),
        1,
        "once per process, not once per read"
    );
}

/// The OTHER half of the split: a read error that is NOT the tier refusal must
/// answer 500, never 503. A blanket-503 edit would tell the operator to run a
/// newer clauth against a full disk. The only production trigger is a home that
/// will not resolve; no harness reaches that (the sandbox panics first), so the
/// arm is driven through the test seam.
#[test]
fn the_route_answers_internal_for_a_read_error_that_is_not_the_tier() {
    let _home = HomeSandbox::new();
    let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ));
    let status_path = token_path().expect("path").with_file_name("status.json");
    let ctx = crate::daemon::api::routes::ApiContext::new(
        config,
        status_path,
        AuthToken::from_plaintext(&"e".repeat(64)),
        None,
    );

    fail_next_home_once();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let resp = crate::daemon::api::routes::handle(
        &ctx,
        &crate::daemon::api::http::Request {
            method: "GET".to_string(),
            path: "/api/v1/health".to_string(),
            query: String::new(),
            bearer: Some("e".repeat(64)),
            if_none_match: None,
            body: Vec::new(),
            keep_alive: true,
        },
    );
    assert_eq!(resp.status, 500, "a non-tier read failure is internal");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&resp.body).expect("json")["error"],
        serde_json::json!("internal")
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.contains("until the token file is readable"))
            .count(),
        1,
        "the operator gets one line naming the failing read, not zero"
    );
}

#[test]
fn verify_accepts_the_exact_token_only() {
    let token = generate().expect("generate");
    let auth = AuthToken::from_plaintext(&token);

    assert!(auth.verify(&token));
    assert!(!auth.verify(""), "empty must not pass");
    assert!(!auth.verify(&token[..63]), "a truncation must not pass");
    assert!(
        !auth.verify(&format!("{token}x")),
        "a token with a suffix must not pass"
    );

    // Flip the last character: the digest compare means a near-miss is no
    // closer to passing than a wildly wrong value.
    let mut near = token.clone();
    let last = if near.ends_with('a') { 'b' } else { 'a' };
    near.pop();
    near.push(last);
    assert!(!auth.verify(&near));
}

/// The token must not be reachable through a formatter. `AuthToken` holds a
/// digest, and a digest still verifies a guess offline.
#[test]
fn auth_token_debug_hides_the_digest() {
    let auth = AuthToken::from_plaintext(&generate().expect("generate"));
    assert_eq!(format!("{auth:?}"), "AuthToken(<redacted>)");
}
