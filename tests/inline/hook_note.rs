#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use crate::testutil::HomeSandbox;

/// A payload carrying only what these tests vary.
fn payload(event: &str, session: &str) -> Payload {
    Payload {
        event: event.to_string(),
        session_id: session.to_string(),
        agent_id: None,
        source: None,
        transcript: None,
    }
}

/// A stamp set whose two halves move INDEPENDENTLY, so a test can pin which
/// input the gate actually watches. The old helper hardcoded the second half,
/// which meant no fixture could tell the two apart and deleting one of them
/// survived the whole suite.
fn watch(creds: u64, config: u64) -> Watch {
    Watch {
        creds: Some(Stamp {
            mtime: SystemTime::UNIX_EPOCH,
            len: creds,
        }),
        config,
    }
}

/// A stamp set serde cannot write: `SystemTime` before the epoch fails to
/// serialize, which is the cheapest way to force `store_record` to error.
fn unwritable_watch() -> Watch {
    Watch {
        creds: Some(Stamp {
            mtime: SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1),
            len: 1,
        }),
        config: 9,
    }
}

fn kerry() -> Option<String> {
    Some("kerry".to_string())
}

fn cld() -> Option<String> {
    Some("cld".to_string())
}

/// clauth cannot attribute the loaded credentials.
fn unknown() -> Option<String> {
    None
}

const SWITCHED: &str =
    "clauth note: the active profile for this session switched from `kerry` to `cld`.";

/// The shipped copy, byte for byte. 25 and 22 tokens against opus-4-8, counted
/// with the literal placeholders `old` and `new` in place of the two account
/// names rather than the ones shown here, so a reworded one is a re-count.
#[test]
fn both_note_spellings_render_the_shipped_copy() {
    assert_eq!(
        Note::Resumed {
            now: "DS4",
            before: "z.ai",
        }
        .render(),
        "clauth note: session resumed under `DS4`; earlier turns ran under `z.ai`.",
    );
    assert_eq!(
        Note::Switched {
            from: "kerry",
            to: "cld",
        }
        .render(),
        SWITCHED,
    );
}

/// Two events, so a hard-coded name cannot pass: the host routes the context by
/// this field, and all three registrations run one binary.
#[test]
fn the_envelope_echoes_the_event_that_produced_it() {
    for event in ["PostToolUse", "UserPromptSubmit", "SessionStart"] {
        assert_eq!(
            envelope(event, "note"),
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": event,
                    "additionalContext": "note",
                }
            }),
        );
    }
}

#[test]
fn the_first_fire_is_a_baseline_and_a_move_is_announced_once() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-1");

    assert_eq!(
        note_for(&fire, &watch(1, 0), &kerry),
        None,
        "there are no earlier turns to correct on a first fire",
    );
    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
    );
    assert_eq!(
        note_for(&fire, &watch(3, 0), &cld),
        None,
        "a fire on the account already told repeats nothing",
    );
}

#[test]
fn a_resume_under_another_account_names_the_earlier_turns() {
    let _home = HomeSandbox::new();
    note_for(
        &payload("UserPromptSubmit", "conv-2"),
        &watch(1, 0),
        &|| Some("z.ai".to_string()),
    );

    let mut resumed = payload("SessionStart", "conv-2");
    resumed.source = Some("resume".to_string());

    assert_eq!(
        note_for(&resumed, &watch(2, 0), &|| Some("DS4".to_string())).as_deref(),
        Some("clauth note: session resumed under `DS4`; earlier turns ran under `z.ai`."),
    );
}

/// The record has to outlive the process that wrote it: a resume is exactly a
/// fresh process on the same conversation id.
#[test]
fn the_record_is_left_on_disk_for_the_next_process() {
    let _home = HomeSandbox::new();
    note_for(
        &payload("UserPromptSubmit", "conv-3"),
        &watch(1, 0),
        &|| Some("z.ai".to_string()),
    );

    let stored =
        load_record(&record_path("conv-3", None).expect("record path")).expect("a record on disk");

    assert_eq!(stored.told.as_deref(), Some("z.ai"));
}

#[test]
fn the_stat_gate_skips_the_resolution_until_a_watched_input_moves() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-4");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 1, "a first fire has nothing cached to gate on");

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(
        calls.get(),
        1,
        "an unmoved stamp must not reach the resolution",
    );

    note_for(&fire, &watch(2, 0), &resolve);
    assert_eq!(calls.get(), 2, "a moved stamp must reach it");
}

/// A single per-conversation flag would let whichever scope fires first consume
/// the note, leaving the other believing the old account.
#[test]
fn a_subagent_and_the_main_thread_each_hear_the_same_move() {
    let _home = HomeSandbox::new();
    let main = payload("UserPromptSubmit", "conv-5");
    note_for(&main, &watch(1, 0), &kerry);

    let mut sub = payload("PostToolUse", "conv-5");
    sub.agent_id = Some("a4a894a1be41b92bf".to_string());

    assert_eq!(
        note_for(&sub, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "the subagent inherits the conversation's baseline, so it hears the move",
    );
    assert_eq!(
        note_for(&main, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "and the main thread still hears it",
    );
}

/// Compaction drops injected context while the record would suppress a second
/// note, which would leave the conversation believing the old account.
#[test]
fn a_compaction_re_announces_the_note_it_dropped() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-6");
    note_for(&fire, &watch(1, 0), &kerry);
    note_for(&fire, &watch(2, 0), &cld).expect("the move is announced");

    let mut compacted = payload("SessionStart", "conv-6");
    compacted.source = Some("compact".to_string());

    assert_eq!(
        note_for(&compacted, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
    );
}

#[test]
fn a_compaction_with_nothing_ever_announced_stays_silent() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-7");
    note_for(&fire, &watch(1, 0), &kerry);

    let mut compacted = payload("SessionStart", "conv-7");
    compacted.source = Some("compact".to_string());

    assert_eq!(note_for(&compacted, &watch(1, 0), &kerry), None);
}

/// A startup or a clear rebaselines rather than announcing: neither context
/// holds an earlier turn to correct.
#[test]
fn a_startup_or_cleared_context_rebaselines_silently() {
    let _home = HomeSandbox::new();
    for source in ["startup", "clear"] {
        let session = format!("conv-8-{source}");
        note_for(&payload("UserPromptSubmit", &session), &watch(1, 0), &kerry);

        let mut started = payload("SessionStart", &session);
        started.source = Some(source.to_string());

        assert_eq!(
            note_for(&started, &watch(2, 0), &cld),
            None,
            "{source} must not announce",
        );
        assert_eq!(
            note_for(&payload("PostToolUse", &session), &watch(2, 0), &cld),
            None,
            "{source} must have moved the baseline it stayed silent about",
        );
    }
}

/// An unattributable credential is not evidence that anything moved, and the
/// name told last has to survive it or the recovery renders a shrug.
#[test]
fn an_unattributable_account_says_nothing_and_keeps_the_name_it_told() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-9");
    note_for(&fire, &watch(1, 0), &kerry);

    assert_eq!(note_for(&fire, &watch(2, 0), &unknown), None);
    assert_eq!(
        note_for(&fire, &watch(3, 0), &cld).as_deref(),
        Some(SWITCHED),
        "the recovery still renders both real names",
    );
}

#[test]
fn an_id_that_cannot_spell_a_bare_filename_is_refused() {
    let ok = r#"{"hook_event_name":"PostToolUse","session_id":"0ee5e2ad-04b3"}"#;
    assert!(parse_payload(ok).is_some(), "a real session id parses");

    for bad in [
        r#"{"hook_event_name":"PostToolUse","session_id":"../../escape"}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"a/b"}"#,
        // A dot is the separator between the two record shapes, so admitting one
        // would let a conversation id spell a subagent's file.
        r#"{"hook_event_name":"PostToolUse","session_id":"a.b"}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":"../x"}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":"a.b"}"#,
    ] {
        assert!(parse_payload(bad).is_none(), "must refuse {bad}");
    }
}

#[cfg(unix)]
#[test]
fn a_conversation_record_and_its_dir_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let _home = HomeSandbox::new();
    note_for(
        &payload("UserPromptSubmit", "conv-10"),
        &watch(1, 0),
        &kerry,
    );

    let path = record_path("conv-10", None).expect("record path");
    let file = std::fs::metadata(&path).expect("the record");
    let dir = std::fs::metadata(path.parent().expect("a parent")).expect("the dir");

    assert_eq!(file.permissions().mode() & 0o777, 0o600);
    assert_eq!(dir.permissions().mode() & 0o777, 0o700);
}

#[test]
fn the_sweep_reaps_a_record_whose_transcript_is_gone() {
    let home = HomeSandbox::new();
    let live = home.home().join("live.jsonl");
    std::fs::write(&live, b"{}").expect("write a transcript");

    let mut kept = payload("UserPromptSubmit", "conv-live");
    kept.transcript = Some(live);
    note_for(&kept, &watch(1, 0), &kerry);

    let mut gone = payload("UserPromptSubmit", "conv-gone");
    gone.transcript = Some(home.home().join("gone.jsonl"));
    note_for(&gone, &watch(1, 0), &kerry);
    // Aged past the grace deliberately. The grace covers a transcript that has
    // not appeared YET (pinned separately); this is the case it must not
    // protect, a transcript that is genuinely gone.
    crate::testutil::set_mtime(
        &record_path("conv-gone", None).expect("path"),
        SystemTime::now() - MISSING_TRANSCRIPT_GRACE - std::time::Duration::from_secs(60),
    );

    gc_conversation_records();

    assert!(
        record_path("conv-live", None).expect("path").exists(),
        "a conversation whose transcript is still there keeps its record",
    );
    assert!(
        !record_path("conv-gone", None).expect("path").exists(),
        "a record whose transcript is gone is reaped",
    );
}

// ── the gate's input set ────────────────────────────────────────────────────

/// The credential stamp and the config fingerprint are two SEPARATE inputs, and
/// a gate watching only one of them lets a real account change through. Deleting
/// either half used to survive the whole suite, because the only fixture pinned
/// one axis and hardcoded the other.
#[test]
fn the_gate_watches_the_config_fingerprint_and_the_credential_store_apart() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-gate");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };

    note_for(&fire, &watch(1, 1), &resolve);
    assert_eq!(calls.get(), 1, "a first fire has nothing cached");

    note_for(&fire, &watch(1, 1), &resolve);
    assert_eq!(calls.get(), 1, "neither input moved");

    note_for(&fire, &watch(2, 1), &resolve);
    assert_eq!(
        calls.get(),
        2,
        "the credential stamp moving must reach the resolution"
    );

    note_for(&fire, &watch(2, 2), &resolve);
    assert_eq!(
        calls.get(),
        3,
        "the config fingerprint moving must reach it too — a per-profile \
         config.toml changes the answer and touches no other watched file",
    );
}

/// An unattributable read must not bank the stamp move that produced it. The
/// move is what opened the gate, so caching it means nothing ever reopens the
/// gate and the note is lost rather than deferred.
#[test]
fn an_unattributable_read_is_never_cached() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-poison");
    note_for(&fire, &watch(1, 0), &kerry);

    let calls = std::cell::Cell::new(0_u32);
    let unresolvable = || {
        calls.set(calls.get() + 1);
        unknown()
    };
    note_for(&fire, &watch(2, 0), &unresolvable);
    note_for(&fire, &watch(2, 0), &unresolvable);
    assert_eq!(
        calls.get(),
        2,
        "the second fire at the same stamp must resolve again, not read a cached None",
    );

    // The record field, not just the call count. `cache_holds` refuses a cached
    // `None` on the READ side too, so the write-side guard is invisible from the
    // count alone — and `resolved` is what the planned owner-store consumer
    // reads, where a clobbered value is the whole defect rather than a slow path.
    let stored = load_record(&record_path("conv-poison", None).expect("path")).expect("a record");
    assert_eq!(
        stored.resolved.as_deref(),
        Some("kerry"),
        "an unattributed read must not overwrite the last account actually resolved",
    );

    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "and the account it could not read is still announced once it can",
    );
}

/// The stamp is an optimisation; the TTL is the correctness bound. Anything the
/// fingerprint does not cover has to expire rather than stick forever.
#[test]
fn a_resolution_is_retaken_once_the_ttl_expires() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-ttl");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 1);

    let path = record_path("conv-ttl", None).expect("record path");
    let mut record = load_record(&path).expect("a record");
    record.resolved_at = Some(SystemTime::UNIX_EPOCH);
    store_record(&path, &record).expect("backdate the resolution");

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(
        calls.get(),
        2,
        "an expired resolution is retaken even though no watched input moved",
    );
}

/// The record IS the suppression mechanism, so a note it cannot remember would
/// be re-emitted on every tool call for the life of the conversation.
#[test]
fn a_note_that_cannot_be_recorded_is_not_emitted() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-nostore");
    note_for(&fire, &watch(1, 0), &kerry);

    assert_eq!(
        note_for(&fire, &unwritable_watch(), &cld),
        None,
        "the account moved, but the record cannot be written, so nothing is said",
    );
    assert_eq!(
        load_record(&record_path("conv-nostore", None).expect("path")).and_then(|r| r.told),
        Some("kerry".to_string()),
        "and the record still holds the account it last managed to remember",
    );
}

// ── the sweep ───────────────────────────────────────────────────────────────

/// A baseline written at `SessionStart` can land before Claude Code creates the
/// transcript. A bare `!exists()` reaps it, and the conversation's next real
/// move is then absorbed as a first fire and never announced.
#[test]
fn a_transcript_that_has_not_appeared_yet_keeps_its_record() {
    let home = HomeSandbox::new();
    let mut fire = payload("UserPromptSubmit", "conv-young");
    fire.transcript = Some(home.home().join("not-yet.jsonl"));
    note_for(&fire, &watch(1, 0), &kerry);

    gc_conversation_records();

    let path = record_path("conv-young", None).expect("path");
    assert!(path.exists(), "a record inside the grace window survives");

    crate::testutil::set_mtime(
        &path,
        SystemTime::now() - MISSING_TRANSCRIPT_GRACE - std::time::Duration::from_secs(60),
    );
    gc_conversation_records();
    assert!(
        !path.exists(),
        "and is reaped once it has aged past the grace"
    );
}

/// The dir also holds the lock file. A sweep that reaps it is a sweep deleting
/// the machinery serialising its own writers.
#[test]
fn the_sweep_leaves_everything_that_is_not_a_record_alone() {
    let _home = HomeSandbox::new();
    note_for(&payload("PostToolUse", "conv-keep"), &watch(1, 0), &kerry);
    let lock = records_dir().expect("dir").join(".lock");
    assert!(
        lock.exists(),
        "the fire took the lock, so the file is there"
    );

    crate::testutil::set_mtime(
        &lock,
        SystemTime::now() - ORPHAN_RECORD_MAX_AGE - std::time::Duration::from_secs(60),
    );
    gc_conversation_records();

    assert!(
        lock.exists(),
        "the lock file is not a record and is never reaped"
    );
}

// ── payload edges ───────────────────────────────────────────────────────────

/// Claude Code documents five `SessionStart` sources and may add more. An
/// unrecognised one must rebaseline, never announce a switch about turns a fresh
/// context never held.
#[test]
fn an_unrecognised_session_start_source_rebaselines_silently() {
    let _home = HomeSandbox::new();
    for source in ["fork", "startup", "clear", "something-claude-adds-later"] {
        let session = format!("conv-src-{source}");
        note_for(&payload("UserPromptSubmit", &session), &watch(1, 0), &kerry);

        let mut started = payload("SessionStart", &session);
        started.source = Some(source.to_string());

        assert_eq!(
            note_for(&started, &watch(2, 0), &cld),
            None,
            "{source} must not announce",
        );
        assert_eq!(
            note_for(&payload("PostToolUse", &session), &watch(2, 0), &cld),
            None,
            "{source} must have moved the baseline it stayed silent about",
        );
    }
}

/// A compaction arriving before anything was ever told has nothing to
/// re-announce, and must still leave the scope with a baseline.
#[test]
fn a_compaction_before_any_baseline_establishes_one() {
    let _home = HomeSandbox::new();
    let mut compacted = payload("SessionStart", "conv-early-compact");
    compacted.source = Some("compact".to_string());

    assert_eq!(note_for(&compacted, &watch(1, 0), &kerry), None);
    assert_eq!(
        note_for(
            &payload("PostToolUse", "conv-early-compact"),
            &watch(2, 0),
            &cld
        )
        .as_deref(),
        Some(SWITCHED),
        "the compaction left a baseline, so the next real move is announced",
    );
}

/// A resume on the SAME account must drop the previous process's note, or a
/// later compaction re-announces a switch this context never saw — every time.
#[test]
fn a_resume_on_the_same_account_drops_the_previous_processes_note() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-carry");
    note_for(&fire, &watch(1, 0), &kerry);
    note_for(&fire, &watch(2, 0), &cld).expect("the move is announced");

    let mut resumed = payload("SessionStart", "conv-carry");
    resumed.source = Some("resume".to_string());
    assert_eq!(
        note_for(&resumed, &watch(2, 0), &cld),
        None,
        "same account, silent"
    );

    let mut compacted = payload("SessionStart", "conv-carry");
    compacted.source = Some("compact".to_string());
    assert_eq!(
        note_for(&compacted, &watch(2, 0), &cld),
        None,
        "the note belonged to a process this context never saw",
    );
}

/// Keyed on the field being absent, never on `as_str()` succeeding: a
/// present-but-unusable value used to read as absent and consume the main
/// thread's record.
#[test]
fn a_present_but_unusable_agent_id_is_refused() {
    for bad in [
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":12345}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":true}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":{}}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":[]}"#,
    ] {
        assert!(parse_payload(bad).is_none(), "must refuse {bad}");
    }
    let absent = r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":null}"#;
    assert!(
        parse_payload(absent).is_some_and(|p| p.agent_id.is_none()),
        "an explicit null is absent, which is the main thread",
    );
}

/// The event name is echoed into the envelope, so it is bounded like both ids.
/// Bounded because it is echoed back, but NOT held to the id charset: that
/// value never reaches a filename, and sharing the charset would take the hook
/// silently offline for any event Claude Code ever namespaces.
#[test]
fn an_event_name_is_bounded_without_being_held_to_the_id_charset() {
    let huge = "A".repeat(65);
    for bad in [
        format!(r#"{{"hook_event_name":"{huge}","session_id":"ok-1"}}"#),
        r#"{"hook_event_name":"","session_id":"ok-1"}"#.to_string(),
        r#"{"hook_event_name":"Post\nToolUse","session_id":"ok-1"}"#.to_string(),
    ] {
        assert!(parse_payload(&bad).is_none(), "must refuse {bad}");
    }
    for ok in [
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1"}"#,
        r#"{"hook_event_name":"a.b","session_id":"ok-1"}"#,
        r#"{"hook_event_name":"a:b","session_id":"ok-1"}"#,
    ] {
        assert!(parse_payload(ok).is_some(), "must accept {ok}");
    }
}

#[test]
fn the_bare_id_bounds_are_exactly_sixty_four_bytes_and_non_empty() {
    assert!(is_bare_id(&"a".repeat(64)), "64 bytes is the last accepted");
    assert!(!is_bare_id(&"a".repeat(65)), "65 is one too many");
    assert!(!is_bare_id(""), "empty spells no filename");
}

/// `watch_now` is the PRODUCTION side of the gate, and it had no test caller at
/// all: every fixture handed `note_for` a `Watch` it built itself, so deleting
/// half of what `watch_now` stamps survived the whole suite. These two drive it
/// against a real tree instead.
#[test]
fn watch_now_sees_a_per_profile_config_edit() {
    let home = HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/acme");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let config = profile.join("config.toml");
    std::fs::write(&config, b"# one\n").expect("write config");

    let before = watch_now();
    std::fs::write(&config, b"# two\n").expect("rewrite config");
    // An explicit future mtime rather than a second write: two writes inside one
    // coarse filesystem tick leave the fingerprint equal and the test would pass
    // for the wrong reason.
    crate::testutil::set_mtime(
        &config,
        SystemTime::now() + std::time::Duration::from_secs(5),
    );
    let after = watch_now();

    assert_ne!(
        before.config, after.config,
        "a per-profile config.toml edit changes the attributed account and \
         touches no other watched file, so the fingerprint must move",
    );
    assert_eq!(
        before.creds, after.creds,
        "and it must not be smuggled in through the credential stamp",
    );
}

#[test]
fn watch_now_sees_the_credential_store_move() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).expect("claude dir");
    let creds = claude.join(".credentials.json");
    std::fs::write(&creds, b"{}").expect("write creds");

    let before = watch_now();
    std::fs::write(&creds, b"{\"a\":1}").expect("rewrite creds");
    crate::testutil::set_mtime(
        &creds,
        SystemTime::now() + std::time::Duration::from_secs(5),
    );
    let after = watch_now();

    assert_ne!(
        before.creds, after.creds,
        "the store this session authenticates from must be stamped",
    );
    assert_eq!(
        before.config, after.config,
        "and it must not be smuggled in through the config fingerprint",
    );
}

/// The no-transcript arm of the sweep. Nothing can test such a record for
/// liveness, so it ages out — and no fixture reached this branch at all, which
/// let both a disabled reap and an inverted comparison survive the whole suite.
#[test]
fn a_record_that_never_carried_a_transcript_ages_out() {
    let _home = HomeSandbox::new();
    // No `transcript` on the payload, which is the only way to reach the arm.
    note_for(&payload("PostToolUse", "conv-orphan"), &watch(1, 0), &kerry);
    let path = record_path("conv-orphan", None).expect("path");
    assert_eq!(
        load_record(&path).and_then(|r| r.transcript),
        None,
        "the fixture has to actually reach the no-transcript branch",
    );

    gc_conversation_records();
    assert!(path.exists(), "a young orphan is kept");

    crate::testutil::set_mtime(
        &path,
        SystemTime::now() - ORPHAN_RECORD_MAX_AGE - std::time::Duration::from_secs(60),
    );
    gc_conversation_records();
    assert!(!path.exists(), "and an aged one is reaped");
}

/// An empty or relative `transcript_path` is dropped at the boundary rather
/// than stored: `Path::new("").exists()` is false, so storing it verbatim aimed
/// the sweep at a live conversation's record.
#[test]
fn an_unusable_transcript_path_is_not_stored() {
    for payload_json in [
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":""}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":"rel/x.jsonl"}"#,
    ] {
        let parsed = parse_payload(payload_json).expect("the payload itself is fine");
        assert_eq!(parsed.transcript, None, "must drop {payload_json}");
    }
    // One "good" fixture per platform: std documents `Path::is_absolute` as
    // prefix-plus-root on Windows ("c:\windows is absolute, c:temp and \temp
    // are not"), so `/a/b.jsonl` has no drive/UNC prefix and drops like the two
    // bad legs there. Both drive the same assertion, so the absolute-positive
    // pin stays covered where the semantics differ.
    let (good, expected) = if cfg!(target_os = "windows") {
        (
            r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":"C:\\a\\b.jsonl"}"#,
            PathBuf::from(r"C:\a\b.jsonl"),
        )
    } else {
        (
            r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":"/a/b.jsonl"}"#,
            PathBuf::from("/a/b.jsonl"),
        )
    };
    assert_eq!(
        parse_payload(good).expect("parses").transcript,
        Some(expected),
    );
}

/// Both sides of the TTL boundary. Backdating to the epoch alone occupies only
/// the far tail, so the constant could move to an hour or a day and nothing
/// would fail.
#[test]
fn the_resolution_ttl_holds_just_inside_and_expires_just_outside() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-ttl-edge");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };
    let backdate = |by: Duration| {
        let path = record_path("conv-ttl-edge", None).expect("path");
        let mut record = load_record(&path).expect("a record");
        record.resolved_at = Some(SystemTime::now() - by);
        store_record(&path, &record).expect("backdate");
    };

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 1);

    // Literal seconds, never `RESOLUTION_TTL ± n`: a margin derived from the
    // constant under test slides with it, so both legs stay on their own side
    // of any value the constant takes and the test can never observe it moving.
    backdate(Duration::from_secs(55));
    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(
        calls.get(),
        1,
        "55s is inside the 60s TTL and still serves the cache"
    );

    backdate(Duration::from_secs(65));
    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 2, "65s is outside it and must resolve again");
}

/// Two concurrent fires resolve OUTSIDE the hold, so the order they reach the
/// lock in says nothing about the order they observed in. The one carrying the
/// older reading must defer, or it announces the reversal — a switch that never
/// happened — and caches its stale answer for the whole TTL.
#[test]
fn a_fire_carrying_the_older_observation_defers_to_the_record() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-stale");
    note_for(&fire, &watch(1, 0), &kerry);
    note_for(&fire, &watch(2, 0), &cld).expect("the real move is announced once");

    // A peer fire that observed LATER but landed first. It writes from inside
    // this fire's own resolution, which is the only way to put its stamp
    // strictly between this fire's `taken_at` and now — a hand-written FUTURE
    // stamp would instead exercise the clock-step case the guard now refuses.
    let stale_observation_racing_a_peer = || {
        std::thread::sleep(Duration::from_millis(5));
        let path = record_path("conv-stale", None).expect("path");
        let mut peer = load_record(&path).expect("a record");
        peer.resolved = Some("cld".to_string());
        peer.resolved_at = Some(SystemTime::now());
        store_record(&path, &peer).expect("the peer lands first");
        kerry() // this fire's own, older reading
    };

    assert_eq!(
        note_for(&fire, &watch(3, 0), &stale_observation_racing_a_peer),
        None,
        "the stale reading must not announce `cld` -> `kerry`, which never happened",
    );
    assert_eq!(
        load_record(&record_path("conv-stale", None).expect("path"))
            .and_then(|r| r.resolved)
            .as_deref(),
        Some("cld"),
        "and it must not overwrite the fresher answer it lost to",
    );
}

/// A backward clock step leaves `resolved_at` in the future with no peer fire
/// involved. Without the past-instant half of the guard, every later fire defers
/// to it and correct answers are discarded for the size of the step — and the
/// TTL cannot bound that, because this path runs only when `cache_holds` has
/// already rejected the cache.
#[test]
fn a_future_timestamp_from_a_clock_step_does_not_freeze_the_answer() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-clockstep");
    note_for(&fire, &watch(1, 0), &kerry);

    let path = record_path("conv-clockstep", None).expect("path");
    let mut record = load_record(&path).expect("a record");
    record.resolved_at = Some(SystemTime::now() + RESOLUTION_TTL * 5);
    store_record(&path, &record).expect("stamp the future");

    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "a stamp that cannot have been taken yet must not win the comparison",
    );
    assert_eq!(
        load_record(&path).and_then(|r| r.resolved).as_deref(),
        Some("cld"),
        "and the fresh answer must land rather than being thrown away",
    );
}
