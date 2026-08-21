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

/// A stamp set that differs from another by `len` alone, so a test can move the
/// gate without going near a filesystem.
fn watch(len: u64) -> Watch {
    Watch {
        creds: Some(Stamp {
            mtime: SystemTime::UNIX_EPOCH,
            len,
        }),
        state: None,
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
    "clauth note: the active profile for this conversation switched from `kerry` to `cld`.";

/// The shipped copy, byte for byte. Both spellings are 24 and 28 tokens against
/// opus-4-8 with these names, so a reworded one is a re-count.
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
        note_for(&fire, &watch(1), &kerry),
        None,
        "there are no earlier turns to correct on a first fire",
    );
    assert_eq!(note_for(&fire, &watch(2), &cld).as_deref(), Some(SWITCHED),);
    assert_eq!(
        note_for(&fire, &watch(3), &cld),
        None,
        "a fire on the account already told repeats nothing",
    );
}

#[test]
fn a_resume_under_another_account_names_the_earlier_turns() {
    let _home = HomeSandbox::new();
    note_for(&payload("UserPromptSubmit", "conv-2"), &watch(1), &|| {
        Some("z.ai".to_string())
    });

    let mut resumed = payload("SessionStart", "conv-2");
    resumed.source = Some("resume".to_string());

    assert_eq!(
        note_for(&resumed, &watch(2), &|| Some("DS4".to_string())).as_deref(),
        Some("clauth note: session resumed under `DS4`; earlier turns ran under `z.ai`."),
    );
}

/// The record has to outlive the process that wrote it: a resume is exactly a
/// fresh process on the same conversation id.
#[test]
fn the_record_is_left_on_disk_for_the_next_process() {
    let _home = HomeSandbox::new();
    note_for(&payload("UserPromptSubmit", "conv-3"), &watch(1), &|| {
        Some("z.ai".to_string())
    });

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

    note_for(&fire, &watch(1), &resolve);
    assert_eq!(calls.get(), 1, "a first fire has nothing cached to gate on");

    note_for(&fire, &watch(1), &resolve);
    assert_eq!(
        calls.get(),
        1,
        "an unmoved stamp must not reach the resolution",
    );

    note_for(&fire, &watch(2), &resolve);
    assert_eq!(calls.get(), 2, "a moved stamp must reach it");
}

/// A single per-conversation flag would let whichever scope fires first consume
/// the note, leaving the other believing the old account.
#[test]
fn a_subagent_and_the_main_thread_each_hear_the_same_move() {
    let _home = HomeSandbox::new();
    let main = payload("UserPromptSubmit", "conv-5");
    note_for(&main, &watch(1), &kerry);

    let mut sub = payload("PostToolUse", "conv-5");
    sub.agent_id = Some("a4a894a1be41b92bf".to_string());

    assert_eq!(
        note_for(&sub, &watch(2), &cld).as_deref(),
        Some(SWITCHED),
        "the subagent inherits the conversation's baseline, so it hears the move",
    );
    assert_eq!(
        note_for(&main, &watch(2), &cld).as_deref(),
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
    note_for(&fire, &watch(1), &kerry);
    note_for(&fire, &watch(2), &cld).expect("the move is announced");

    let mut compacted = payload("SessionStart", "conv-6");
    compacted.source = Some("compact".to_string());

    assert_eq!(
        note_for(&compacted, &watch(2), &cld).as_deref(),
        Some(SWITCHED),
    );
}

#[test]
fn a_compaction_with_nothing_ever_announced_stays_silent() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-7");
    note_for(&fire, &watch(1), &kerry);

    let mut compacted = payload("SessionStart", "conv-7");
    compacted.source = Some("compact".to_string());

    assert_eq!(note_for(&compacted, &watch(1), &kerry), None);
}

/// A startup or a clear rebaselines rather than announcing: neither context
/// holds an earlier turn to correct.
#[test]
fn a_startup_or_cleared_context_rebaselines_silently() {
    let _home = HomeSandbox::new();
    for source in ["startup", "clear"] {
        let session = format!("conv-8-{source}");
        note_for(&payload("UserPromptSubmit", &session), &watch(1), &kerry);

        let mut started = payload("SessionStart", &session);
        started.source = Some(source.to_string());

        assert_eq!(
            note_for(&started, &watch(2), &cld),
            None,
            "{source} must not announce",
        );
        assert_eq!(
            note_for(&payload("PostToolUse", &session), &watch(2), &cld),
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
    note_for(&fire, &watch(1), &kerry);

    assert_eq!(note_for(&fire, &watch(2), &unknown), None);
    assert_eq!(
        note_for(&fire, &watch(3), &cld).as_deref(),
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
    note_for(&payload("UserPromptSubmit", "conv-10"), &watch(1), &kerry);

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
    note_for(&kept, &watch(1), &kerry);

    let mut gone = payload("UserPromptSubmit", "conv-gone");
    gone.transcript = Some(home.home().join("gone.jsonl"));
    note_for(&gone, &watch(1), &kerry);

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
