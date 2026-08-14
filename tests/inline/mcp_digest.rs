#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The since-your-last-call digest, pinned on the traps that make it a
//! feature rather than a no-op:
//!
//! - the baseline is SHARED across server clones (rmcp clones the handler per
//!   request; a per-clone baseline reports nothing forever);
//! - a first call reports nothing (there was no earlier state to compare
//!   against, and claiming "nothing changed" would assert otherwise);
//! - reporting consumes the delta, and a surface that does not report must not
//!   swallow it (`list_profiles`, a filtered `watch`);
//! - `switch` never reports its own write (it reseeds), but an arm that
//!   refused before any mutation reports like `which` does;
//! - `watch` returns as soon as something moves, never sleeps holding the
//!   baseline lock, and honors its `kinds` filter;
//! - the usage cache is keyed on the profile it was read from, so a profile
//!   change is never dressed up as a refresh of a file nobody refreshed;
//! - a batch is one call: one digest, top-level, rendered in the prose that is
//!   the default format.

use super::*;
use crate::profile::{AppState, ProfileName, save_app_state};
use crate::profile_cache::USAGE_CACHE_FILE;
use crate::testutil::{HomeSandbox, set_mtime};
use crate::usage::UsageInfo;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A fixed old stamp and its successor: distinct values, so a `set_mtime` move
/// can never collide with a same-instant write. (`SystemTime + Duration` is
/// not a const operation, so these are fns.)
fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn t1() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_010)
}

fn credentials_path() -> std::path::PathBuf {
    crate::claude::claude_credentials_path().expect("credentials path")
}

fn seed_credentials_file(at: SystemTime) {
    let path = credentials_path();
    std::fs::create_dir_all(path.parent().expect("parent")).expect("claude dir");
    std::fs::write(&path, b"{}").expect("credentials file");
    set_mtime(&path, at);
}

/// Persist `active` as the configured active profile and give it a usage
/// cache stamped at `at`. The digest reads the raw state value, so `active`
/// need not name a stored profile unless the test drives `switch`.
fn seed_state(active: &str, at: SystemTime) {
    save_app_state(&AppState {
        active_profile: Some(ProfileName::from(active)),
        profiles: vec![ProfileName::from(active)],
        ..Default::default()
    })
    .expect("save state");
    crate::profile_cache::write_profile_cache(active, USAGE_CACHE_FILE, &UsageInfo::default());
    let cache = crate::profile_cache::profile_cache_path(active, USAGE_CACHE_FILE)
        .expect("usage cache path");
    set_mtime(&cache, at);
}

fn drive<F>(fut: F) -> CallToolResult
where
    F: std::future::Future<Output = Result<CallToolResult, ErrorData>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(fut)
        .expect("tool returns a tool result, never a transport error")
}

fn block_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("payload text")
}

fn json_payload(result: &CallToolResult) -> serde_json::Value {
    serde_json::from_str(&block_text(result)).expect("parse json payload")
}

fn call_which(server: &ClauthServer) -> serde_json::Value {
    json_payload(&drive(server.which(Parameters(WhichArgs {
        format: Some("json".to_string()),
    }))))
}

fn call_switch(server: &ClauthServer, name: &str) -> serde_json::Value {
    json_payload(&drive(server.switch(Parameters(SwitchArgs {
        name: name.to_string(),
        format: Some("json".to_string()),
    }))))
}

fn call_batch(server: &ClauthServer, job_ids: &[&str], format: &str) -> CallToolResult {
    drive(server.delegate_result(Parameters(DelegateResultArgs {
        job_id: None,
        job_ids: Some(job_ids.iter().map(|s| (*s).to_string()).collect()),
        wait_secs: Some(0),
        format: Some(format.to_string()),
    })))
}

fn call_watch(
    server: &ClauthServer,
    wait_secs: Option<u64>,
    kinds: Option<Vec<&str>>,
    format: Option<&str>,
) -> CallToolResult {
    drive(server.watch(Parameters(WatchArgs {
        wait_secs,
        kinds: kinds.map(|k| k.iter().map(|s| s.to_string()).collect()),
        format: format.map(str::to_string),
    })))
}

fn watch_json(
    server: &ClauthServer,
    wait_secs: Option<u64>,
    kinds: Option<Vec<&str>>,
) -> serde_json::Value {
    json_payload(&call_watch(server, wait_secs, kinds, Some("json")))
}

/// The full fresh-server fixture every test starts from: one active profile,
/// a usage cache, and a credentials file, all stamped at `t0()`.
fn seeded_world() {
    seed_state("work", t0());
    seed_credentials_file(t0());
}

#[test]
fn a_first_digest_call_reports_nothing() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let payload = call_which(&server);
    assert!(
        payload.get("since_your_last_call").is_none(),
        "the first digest call establishes the baseline and must not claim a \
         comparison it never made: {payload}",
    );
}

/// THE sharing trap: rmcp clones the handler per request, so a baseline stored
/// as a plain field gives every clone its own and the feature reports nothing
/// forever. A clone must compare against the ORIGINAL's baseline.
#[test]
fn a_server_clone_shares_the_digest_baseline() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    seed_state("other", t0());
    let clone = server.clone();
    let payload = call_which(&clone);
    assert_eq!(
        payload["since_your_last_call"]["active_profile"],
        serde_json::json!({ "from": "work", "to": "other" }),
        "a clone must see the original's baseline and report what moved: {payload}",
    );
}

#[test]
fn reporting_consumes_the_delta() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    set_mtime(&credentials_path(), t1());
    let second = call_which(&server);
    assert_eq!(second["since_your_last_call"]["credentials"], true);

    let third = call_which(&server);
    assert!(
        third.get("since_your_last_call").is_none(),
        "a reported change is consumed: the third call must not re-report it: {third}",
    );
}

/// Linux reports mtimes in nanoseconds. Truncated to milliseconds, two writes
/// landing inside one millisecond read as one and the second is lost — the
/// mtime-as-change-detector trap this project has paid for before.
#[test]
fn a_sub_millisecond_mtime_move_is_still_a_change() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    let bumped = t0() + Duration::from_micros(500);
    set_mtime(&credentials_path(), bumped);
    // Fixture control: a filesystem that rounds the stamp away leaves no
    // sub-millisecond move to catch, and the assertion below would then pass
    // for the wrong reason.
    assert_eq!(
        std::fs::metadata(credentials_path())
            .expect("credentials metadata")
            .modified()
            .expect("credentials mtime"),
        bumped,
        "the sandbox filesystem must keep the sub-millisecond stamp",
    );

    let payload = call_which(&server);
    assert_eq!(
        payload["since_your_last_call"]["credentials"], true,
        "a write 500µs after the baseline is a write: {payload}",
    );
}

/// `list_profiles` neither carries nor consumes the digest: its roster is
/// already a fresh read of the same state, so a delta beside it buys nothing —
/// and swallowing the delta there would mute it for every later reporter.
#[test]
fn list_profiles_carries_no_digest_and_consumes_nothing() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    set_mtime(&credentials_path(), t1());
    let roster = json_payload(&drive(server.list_profiles(Parameters(ListProfilesArgs {
        names: None,
        format: Some("json".to_string()),
    }))));
    assert!(
        roster.get("since_your_last_call").is_none(),
        "list_profiles has no live footer and no digest: {roster}",
    );

    let payload = call_which(&server);
    assert_eq!(
        payload["since_your_last_call"]["credentials"], true,
        "a list_profiles call between the change and the report must not swallow it",
    );
}

/// Seed two cleanly-linked registered profiles, active + target, the shape a
/// successful switch needs (mirrors the switch-tool suite's fixture). Gated
/// with its only caller below: ungated it is dead code on the Windows leg,
/// which lints at `-D warnings`.
#[cfg(unix)]
fn seed_switchable_pair() {
    use crate::claude::force_link_profile_credentials;
    use crate::profile::{ClaudeCredentials, OAuthToken, Profile, save_profile};

    for name in ["active", "target"] {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: format!("at-{name}"),
                refresh_token: Some(format!("rt-{name}")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        });
        save_profile(&p).expect("save profile");
    }
    force_link_profile_credentials("active").expect("link active");
    save_app_state(&AppState {
        active_profile: Some("active".into()),
        profiles: vec!["active".into(), "target".into()],
        ..Default::default()
    })
    .expect("save state");
}

/// A switch that ran never reports its own write: its reply's
/// `previous`/`active` IS the report, and the reseed means the next call does
/// not echo the switch back as news from elsewhere.
// Unix-gated with the switch-tool suite it mirrors: the mutation path it
// drives is the one that suite keeps off the Windows leg.
#[test]
#[cfg(unix)]
fn a_successful_switch_reseeds_rather_than_reporting_its_own_write() {
    let _home = HomeSandbox::new();
    seed_switchable_pair();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    let switched = call_switch(&server, "target");
    assert_eq!(switched["ok"], true, "fixture control: the switch ran");
    assert_eq!(switched["active"], "target");
    assert!(
        switched.get("since_your_last_call").is_none(),
        "a switch reply must not report its own write as external news: {switched}",
    );

    let after = call_which(&server);
    assert!(
        after.get("since_your_last_call").is_none(),
        "the reseed consumed the switch's write; the next reply must stay silent: {after}",
    );
}

/// A switch that refused BEFORE any mutation ran wrote nothing, so any delta
/// it sees is external news and reports exactly like `which` does.
#[test]
fn a_refused_switch_reports_external_changes() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    seed_state("other", t0());
    let refused = call_switch(&server, "ghost");
    assert_eq!(refused["ok"], false, "fixture control: it refused");
    assert_eq!(
        refused["since_your_last_call"]["active_profile"],
        serde_json::json!({ "from": "work", "to": "other" }),
        "a pre-mutation refusal carries the digest like `which` does: {refused}",
    );
}

#[test]
fn watch_first_call_arms_the_baseline() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let armed = watch_json(&server, Some(0), None);
    assert_eq!(armed["status"], "armed");
    assert!(
        armed.get("since_your_last_call").is_none(),
        "arming is not a comparison: {armed}",
    );
}

/// The long-poll half of the contract: a change landing mid-wait wakes the
/// call at the next poll slice, not at the deadline.
#[test]
fn watch_returns_as_soon_as_something_moves() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    let path = credentials_path();
    let mover = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        set_mtime(&path, t1());
    });
    let start = Instant::now();
    let payload = watch_json(&server, Some(60), None);
    let elapsed = start.elapsed();
    assert_eq!(
        payload["status"], "changed",
        "the mid-wait change must be caught: {payload}",
    );
    assert_eq!(payload["since_your_last_call"]["credentials"], true);
    assert!(
        elapsed < Duration::from_secs(10),
        "a change 300ms in must return at the next poll slice, not the 60s \
         deadline (took {elapsed:?})",
    );
    mover.join().expect("mover thread");
}

/// The `kinds` filter reports only what it watched AND leaves the unwatched
/// observables' changes intact for the next reporter: a filtered watch that
/// stored its whole sample — on its CHANGED arm or its unchanged one — would
/// swallow them. Both arms need a leg of their own.
#[test]
fn watch_kinds_filter_reports_watched_only_and_never_swallows_the_rest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);
    let cache = crate::profile_cache::profile_cache_path("work", USAGE_CACHE_FILE)
        .expect("usage cache path");

    // The UNCHANGED arm: a filtered watch that saw nothing in its own set
    // still must not store the fresh sample, or the usage-cache move dies with
    // its reply.
    set_mtime(&cache, t1());
    let blind = watch_json(&server, Some(0), Some(vec!["credentials"]));
    assert_eq!(blind["status"], "unchanged");

    // The CHANGED arm: a credentials-only watch must consume credentials only,
    // leaving the usage-cache move pending.
    set_mtime(&credentials_path(), t1());
    let credentials_only = watch_json(&server, Some(0), Some(vec!["credentials"]));
    assert_eq!(credentials_only["status"], "changed");
    assert_eq!(
        credentials_only["since_your_last_call"]["credentials"],
        true
    );
    assert!(
        credentials_only["since_your_last_call"]
            .get("usage_cache")
            .is_none(),
        "an unwatched observable carries no news: {credentials_only}",
    );

    let usage_only = watch_json(&server, Some(0), Some(vec!["usage_cache"]));
    assert_eq!(
        usage_only["since_your_last_call"]["usage_cache"], true,
        "neither filtered watch may swallow the usage-cache move",
    );

    // And the filter consumes what it reported.
    let again = watch_json(&server, Some(0), Some(vec!["usage_cache"]));
    assert_eq!(again["status"], "unchanged");
}

/// The usage-cache observable is KEYED on the profile it was read from: two
/// profiles' caches are different files, so a profile change is no
/// `usage_cache` event. A `kinds: ["usage_cache"]` watch hides the profile
/// change, so reporting the incomparable pair as a refresh puts a false lesser
/// event in its place — a statement to the model that nothing made true.
#[test]
fn a_profile_change_is_never_reported_as_a_usage_cache_refresh() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    // Another profile, carrying its own cache at its own stamp.
    seed_state("other", t1());
    let filtered = watch_json(&server, Some(0), Some(vec!["usage_cache"]));
    assert_eq!(
        filtered["status"], "unchanged",
        "the compared mtime came from a different profile's file, so nothing \
         refreshed: {filtered}",
    );

    // The profile change itself survives for the surface that watches it, and
    // still drags no refresh along with it.
    let reported = call_which(&server);
    assert_eq!(
        reported["since_your_last_call"]["active_profile"],
        serde_json::json!({ "from": "work", "to": "other" }),
        "the filtered watch left the profile change for the next reporter: {reported}",
    );
    assert!(
        reported["since_your_last_call"]
            .get("usage_cache")
            .is_none(),
        "two profiles' caches are not comparable: {reported}",
    );

    // Consuming the profile change re-keys the cache baseline, so the false
    // refresh cannot land one call later either.
    let next = call_which(&server);
    assert!(
        next.get("since_your_last_call").is_none(),
        "the re-key onto the new profile's cache is silent: {next}",
    );
}

#[test]
fn watch_timeout_reports_unchanged_with_waited_secs() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_which(&server);

    let start = Instant::now();
    let payload = watch_json(&server, Some(1), None);
    assert_eq!(payload["status"], "unchanged");
    let waited = payload["waited_secs"].as_u64().expect("waited_secs");
    assert!(
        waited >= 1 && start.elapsed() >= Duration::from_secs(1),
        "the wait must actually elapse before the unchanged answer: {payload}",
    );
}

#[test]
fn watch_refuses_an_unknown_kind_by_name() {
    let _home = HomeSandbox::new();
    let server = ClauthServer::new();

    let result = call_watch(&server, None, Some(vec!["credentials", "bogus"]), None);
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("refusal text"),
        r#"{"ok":false,"reason":"unrecognized kind \"bogus\": accepted \"active_profile\", \"usage_cache\", \"credentials\""}"#
    );
}

#[test]
fn watch_answers_prose_by_default() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let armed = call_watch(&server, Some(0), None, None);
    assert_eq!(
        armed
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("armed prose"),
        "watch armed: baseline set on this first digest call, nothing to compare against yet",
    );

    let _ = call_which(&server);
    set_mtime(&credentials_path(), t1());
    let changed = call_watch(&server, Some(0), None, None);
    assert_eq!(
        changed
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("changed prose"),
        "watch: since your last call: credentials file rewritten",
    );
}

/// No lock may span a sleep in the digest machinery: `watch` runs up to 60s,
/// and a baseline lock held across its slices would stall every other
/// digest-bearing reply on the server. The shape is what's checkable — a
/// timing probe stays green under a slice-wise violation, because the mutex
/// futex hands the lock to a parked waiter inside one 200ms slice — so this is
/// a source guard (the out.rs pattern): the sleeping function must not lock,
/// and the locking functions must not sleep.
///
/// Ceiling: it reads `src/mcp/digest.rs` textually, so it catches a lock or a
/// sleep landing in the named function bodies, not one laundered through a
/// fresh helper those bodies call. That requires deliberately adding a helper,
/// which is the point where a review reads the lock order anyway.
#[test]
fn the_sleeping_function_never_locks_and_the_locking_functions_never_sleep() {
    let src = include_str!("../../src/mcp/digest.rs");
    fn body(src: &str, name: &str) -> String {
        let start = src
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("{name} not found"));
        // To the next method at the same indent, private or `pub(super)`.
        let rest = &src[start..];
        let end = ["\n    fn ", "\n    pub(super) fn "]
            .iter()
            .filter_map(|pat| rest.find(pat))
            .min()
            .unwrap_or(rest.len());
        // Comment lines out: the scanned contract is about CODE, and the doc
        // comments around these methods name `sleep` and `lock` in prose.
        rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    let watch = body(src, "watch");
    assert!(
        !watch.contains(".lock()") && !watch.contains("lock("),
        "watch sleeps, so it must not hold the baseline lock anywhere in its \
         body: {watch}",
    );
    for locker in ["report", "reseed"] {
        let body = body(src, locker);
        assert!(
            !body.contains("sleep"),
            "{locker} takes the baseline lock, so it must not sleep inside it: {body}",
        );
    }
}

/// The `delegate_result` done envelope is a digest-bearing reply too (it folds
/// `live_usage`), so it reports and consumes like `which` does.
#[test]
fn delegate_result_done_envelope_reports_the_digest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let envelope = serde_json::json!({
        "profile": "work",
        "is_error": false,
        "result": "all done",
    });
    jobs::write_done("d-digest-0", "work", 1, envelope.clone()).expect("write job");
    let first = json_payload(&drive(server.delegate_result(Parameters(
        DelegateResultArgs {
            job_id: Some("d-digest-0".to_string()),
            job_ids: None,
            wait_secs: Some(0),
            format: Some("json".to_string()),
        },
    ))));
    assert!(
        first.get("since_your_last_call").is_none(),
        "first digest call seeds, even through delegate_result: {first}",
    );

    jobs::write_done("d-digest-1", "work", 1, envelope).expect("write job");
    set_mtime(&credentials_path(), t1());
    let second = json_payload(&drive(server.delegate_result(Parameters(
        DelegateResultArgs {
            job_id: Some("d-digest-1".to_string()),
            job_ids: None,
            wait_secs: Some(0),
            format: Some("json".to_string()),
        },
    ))));
    assert_eq!(
        second["since_your_last_call"]["credentials"], true,
        "the done envelope reports what moved since the first call: {second}",
    );
}

/// Seed one finished job whose envelope reads `all done`.
fn seed_done_job(id: &str) {
    jobs::write_done(
        id,
        "work",
        1,
        serde_json::json!({ "profile": "work", "is_error": false, "result": "all done" }),
    )
    .expect("write job");
}

/// A batch is ONE call, so its digest rides the reply once, top-level beside
/// `results` like every other surface. Folded into each done result instead, it
/// is reported (and consumed) per job, and nests where no other surface puts
/// it.
#[test]
fn delegate_result_batch_carries_one_top_level_digest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    seed_done_job("d-batch-0");
    seed_done_job("d-batch-1");
    let first = json_payload(&call_batch(&server, &["d-batch-0", "d-batch-1"], "json"));
    assert!(
        first.get("since_your_last_call").is_none(),
        "the first digest call seeds, through a batch like anywhere else: {first}",
    );

    set_mtime(&credentials_path(), t1());
    seed_done_job("d-batch-2");
    seed_done_job("d-batch-3");
    let second = json_payload(&call_batch(&server, &["d-batch-2", "d-batch-3"], "json"));
    assert_eq!(
        second["since_your_last_call"]["credentials"], true,
        "the batch reply carries the digest beside `results`: {second}",
    );
    for entry in second["results"].as_array().expect("results is an array") {
        assert!(
            entry.get("since_your_last_call").is_none(),
            "a per-result copy would nest the digest where no reader looks for \
             it, on the first done job alone: {entry}",
        );
    }

    let after = call_which(&server);
    assert!(
        after.get("since_your_last_call").is_none(),
        "the batch reported the change, so the batch consumed it: {after}",
    );
}

/// Prose is the default format and `single_block` emits prose ALONE, so a
/// digest the batch consumes but never renders is lost for good.
#[test]
fn delegate_result_batch_prose_renders_the_digest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    seed_done_job("d-bprose-0");
    let _ = call_batch(&server, &["d-bprose-0"], "json");

    set_mtime(&credentials_path(), t1());
    seed_done_job("d-bprose-1");
    assert_eq!(
        block_text(&call_batch(&server, &["d-bprose-1"], "prose")),
        "job `d-bprose-1` finished: all done\n\
         since your last call: credentials file rewritten",
    );
}
