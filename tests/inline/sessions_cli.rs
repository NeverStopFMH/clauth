//! `clauth sessions/resume/info` CLI surface tests. Fixture stores live under a
//! `HomeSandbox` so the global (`~/.claude/projects`) walk stays off the real
//! tree. Each transcript is named `<sessionId>.jsonl` (the id is the filename
//! stem). Pure helpers (`resume_profile_choice`, `sessions_json`) are exercised
//! directly; the exit-code contract goes through `crate::exit_code`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::json;

use crate::testutil::{HomeSandbox, set_mtime};

fn write_jsonl(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n")).unwrap();
}

fn user_line(sid: &str, cwd: &str, text: &str) -> String {
    json!({"sessionId": sid, "cwd": cwd, "message": {"role": "user", "content": text}}).to_string()
}

/// An assistant usage line — the token-bearing row `file_model_tokens` reads.
fn usage_line(sid: &str, cwd: &str, msg_id: &str, model: &str, input: u64, output: u64) -> String {
    json!({
        "sessionId": sid, "cwd": cwd, "timestamp": "2026-06-11T10:30:00+00:00",
        "message": {
            "id": msg_id, "role": "assistant", "model": model,
            "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0
            }
        }
    })
    .to_string()
}

/// A `PriceTable` from `(model_id, input_rate, output_rate)` rows; cache rates 0.
fn price_table(rows: &[(&str, f64, f64)]) -> crate::pricing::PriceTable {
    let mut rates = HashMap::new();
    for &(id, input, output) in rows {
        rates.insert(
            id.to_owned(),
            crate::pricing::ModelRate {
                input,
                output,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
    }
    crate::pricing::PriceTable::from_rates(rates)
}

// ── clauth sessions --json ──

#[test]
fn sessions_json_has_exact_fields_newest_first_with_null_and_redaction() {
    let sb = HomeSandbox::new();

    // Newer session: a token-bearing usage row + a secret in the first message.
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(
        &a,
        &[
            user_line(
                "aaaa-1111",
                "/ws/a",
                "my key sk-ant-api03-ABCDEFGHIJKLMNOPQRST here",
            ),
            usage_line("aaaa-1111", "/ws/a", "m1", "claude-sonnet-4", 100, 50),
        ],
    );

    // Older session: no usage row, so its token total stays absent (JSON null).
    let b = sb.home().join(".claude/projects/-w-b/bbbb-2222.jsonl");
    write_jsonl(&b, &[user_line("bbbb-2222", "/ws/b", "plain question")]);
    set_mtime(&b, SystemTime::now() - Duration::from_secs(3600));

    let mut groups = crate::sessions::build_index();
    let price = price_table(&[("claude-sonnet-4", 0.000003, 0.000015)]);
    crate::sessions::annotate_all(&mut groups, Some(&price));
    crate::sessions::annotate_owners(&mut groups);
    let flat = flatten_newest_first(&groups);
    let value = sessions_json(&flat);

    let arr = value.as_array().expect("json array");
    assert_eq!(arr.len(), 2, "both sessions present");

    // Newest-first: the token-bearing session (fresh mtime) leads.
    assert_eq!(arr[0]["id"], json!("aaaa-1111"), "newest session first");
    assert_eq!(arr[1]["id"], json!("bbbb-2222"));

    // Exactly the documented field set — no more, no less.
    let keys: std::collections::BTreeSet<&str> = arr[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let want: std::collections::BTreeSet<&str> = [
        "id",
        "last_ran_profile",
        "workspace",
        "updated",
        "first_message",
        "last_message",
        "tokens",
        "cost",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        keys, want,
        "json row must carry exactly the documented fields"
    );

    // Tokenless session ⇒ JSON null, never 0.
    assert_eq!(arr[1]["tokens"], json!(null), "absent tokens must be null");
    assert_eq!(arr[1]["cost"], json!(null), "absent cost must be null");

    // Priced token-bearing session ⇒ a real number, not null.
    assert!(
        arr[0]["tokens"].is_number(),
        "priced session has a token total"
    );
    assert!(arr[0]["cost"].is_number(), "priced session has a cost");

    // `updated` is an ISO-8601 UTC string.
    let updated = arr[0]["updated"].as_str().expect("updated is a string");
    assert!(
        updated.contains('T') && updated.ends_with("+00:00"),
        "updated must be ISO-8601 UTC, got {updated}"
    );

    // Redaction survives into the emitted preview.
    let preview = arr[0]["first_message"]
        .as_str()
        .expect("first_message string");
    assert!(
        preview.contains("[REDACTED]") && !preview.contains("sk-ant-api03-ABCDEFGHIJKLMNOPQRST"),
        "the secret must be redacted in the preview, got {preview}"
    );
}

// ── --tokens gates the full-transcript annotation ──

#[test]
fn the_listing_costs_a_full_read_of_every_transcript_only_under_tokens() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(
        &a,
        &[
            user_line("aaaa-1111", "/ws/a", "hello"),
            usage_line("aaaa-1111", "/ws/a", "m1", "claude-sonnet-4", 100, 50),
        ],
    );

    // Default: the index only, so the figure the annotation would fill stays
    // absent. Its `None` is the same one a session with no usage row gets.
    let plain = build_listing(false);
    let row = &plain[0].sessions[0];
    assert_eq!(row.tokens, None, "no annotation ⇒ no token total");
    assert_eq!(
        row.first_message.as_deref(),
        Some("hello"),
        "the preview still comes from the index, which the flag never gates"
    );

    // Opt in and the same store yields a real total — so the `None` above is
    // the flag's doing, not a fixture that had nothing to report. Cost stays
    // absent either way here: the sandbox has no price cache, and a cold cache
    // prices nothing.
    let annotated = build_listing(true);
    let row = &annotated[0].sessions[0];
    assert_eq!(row.tokens, Some(150), "--tokens sums input+output");
}

#[test]
fn the_table_carries_the_token_columns_only_under_tokens() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(
        &a,
        &[
            user_line("aaaa-1111", "/ws/a", "hello"),
            usage_line("aaaa-1111", "/ws/a", "m1", "claude-sonnet-4", 100, 50),
        ],
    );
    // A fixed mtime keeps the timestamp cell out of the substring assertions.
    set_mtime(&a, SystemTime::UNIX_EPOCH + Duration::from_secs(3_000));

    // Annotated through the same call the flag makes, so the row has real
    // figures to print and a missing column is the flag's doing, not an empty
    // fixture rendering blank either way.
    let mut groups = build_listing(false);
    crate::sessions::annotate_all(
        &mut groups,
        Some(&price_table(&[("claude-sonnet-4", 0.000003, 0.000015)])),
    );
    let session = &groups[0].sessions[0];

    let plain = session_row(session, false);
    assert!(
        !plain.contains("150") && !plain.contains('$'),
        "the default row carries neither figure: {plain}"
    );
    assert!(plain.contains("hello"), "it still carries the preview");

    let annotated = session_row(session, true);
    assert!(
        annotated.contains("150") && annotated.contains("$0."),
        "--tokens adds both cells: {annotated}"
    );
}

// ── exit-code contract (0 / 1 / 2) ──

#[test]
fn no_sessions_found_maps_to_exit_one() {
    let _sb = HomeSandbox::new(); // empty tree ⇒ empty index
    let err = run_sessions(true, false).expect_err("empty index must error");
    assert!(
        err.to_string().contains("no sessions"),
        "error must say no sessions were found: {err}"
    );
    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "an empty index is a runtime error, not a usage error"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

#[test]
fn sessions_bad_flag_maps_to_exit_two() {
    // Through the real grammar: an unknown `sessions` flag never reaches
    // dispatch, and clap's own parse-failure code is the same 2 the
    // sessions-surface `UsageError` maps to, so the contract holds either way.
    use clap::Parser as _;
    let err = crate::cli::Cli::try_parse_from(["clauth", "sessions", "--bogus"])
        .expect_err("bad flag must error");
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn success_maps_to_exit_zero() {
    assert_eq!(crate::exit_code(Ok(())), 0);
}

// ── resume_profile_choice: the four branches ──

#[test]
fn resume_profile_choice_explicit_flag_forces_no_prompt() {
    // A flag wins regardless of tty or a known last-ran profile.
    assert_eq!(
        resume_profile_choice(Some("chosen"), true, Some("lastran"), "active"),
        ("chosen".to_string(), false)
    );
    assert_eq!(
        resume_profile_choice(Some("chosen"), false, None, "active"),
        ("chosen".to_string(), false)
    );
}

#[test]
fn resume_profile_choice_piped_no_flag_uses_active_forced() {
    assert_eq!(
        resume_profile_choice(None, false, Some("lastran"), "active"),
        ("active".to_string(), false)
    );
}

#[test]
fn resume_profile_choice_tty_known_last_ran_prompts_defaulting_to_it() {
    // Mutation target: if this branch returned `active`, this test fails.
    assert_eq!(
        resume_profile_choice(None, true, Some("lastran"), "active"),
        ("lastran".to_string(), true)
    );
}

#[test]
fn resume_profile_choice_tty_unknown_prompts_defaulting_to_active() {
    assert_eq!(
        resume_profile_choice(None, true, None, "active"),
        ("active".to_string(), true)
    );
}

// ── resolve_session: the targeted lookup behind resume/info ──

/// `resume latest` and the emitted listing must name the same session whenever
/// the listing's first row is one a resume can open. They read the store two
/// different ways now — a filename-and-mtime walk vs the full index — so the
/// agreement is a real invariant, not a tautology. The one exception is pinned by
/// `newest_session_skips_the_nested_transcripts_a_resume_cannot_open`.
#[test]
fn latest_resolves_to_the_first_row_the_listing_emits() {
    let sb = HomeSandbox::new();
    let older = sb.home().join(".claude/projects/-w-a/s-older.jsonl");
    let newer = sb.home().join(".claude/projects/-w-b/s-newer.jsonl");
    write_jsonl(&older, &[user_line("s-older", "/ws/a", "older")]);
    write_jsonl(&newer, &[user_line("s-newer", "/ws/b", "newer")]);
    set_mtime(&older, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&newer, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));

    let groups = build_listing(false);
    let flat = flatten_newest_first(&groups);
    let Resolved::Ready(latest) = resolve_session("latest") else {
        panic!("`latest` must resolve against a store with no live isolated run");
    };
    assert_eq!(
        Some(latest.id.as_str()),
        flat.first().map(|s| s.id.as_str()),
        "`latest` must be the session `clauth sessions` lists first"
    );
    assert_eq!(latest.id, "s-newer");
}

/// Seed a live isolated runtime for `profile` holding one transcript, and return
/// its lock file — the runtime reads as live only while that stays held.
fn live_isolated_session(
    sb: &HomeSandbox,
    profile: &str,
    id: &str,
    workspace: &str,
    mtime_secs: u64,
) -> std::fs::File {
    let iso = sb.home().join(format!(
        ".clauth/profiles/{profile}/runtime-isolated/projects/-w-iso/{id}.jsonl"
    ));
    write_jsonl(&iso, &[user_line(id, workspace, "hi iso")]);
    set_mtime(
        &iso,
        SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
    );
    let sessions_dir = sb
        .home()
        .join(format!(".clauth/profiles/{profile}/sessions-isolated"));
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap();
    lock_file
}

/// An id that only exists in a live isolated runtime is not resumable: the spawn
/// is `Isolation::Shared`, so Claude Code would look in the shared store and
/// answer `No conversation found`. Refusing it is right; calling it missing is
/// not, when `clauth sessions` lists it two lines up.
#[test]
fn resume_refuses_an_isolated_held_session_without_calling_it_missing() {
    let sb = HomeSandbox::new();
    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let lock_file = live_isolated_session(&sb, "iso", "siso", &ws.to_string_lossy(), 5_000);

    // The listing still browses it — only the resume path refuses.
    assert!(
        flatten_newest_first(&build_listing(false))
            .iter()
            .any(|s| s.id == "siso"),
        "the listing covers a live isolated store"
    );

    let err = run_resume("siso", None).expect_err("an isolated-held id must be refused");
    drop(lock_file);
    let msg = err.to_string();
    assert!(
        msg.contains("'siso'") && msg.contains("profile 'iso'"),
        "the refusal must name the session and the run holding it: {msg}"
    );
    assert!(
        !msg.contains("no session found"),
        "a listed session is not missing: {msg}"
    );
    assert!(
        msg.contains("--rescue"),
        "and must say what makes it reachable: {msg}"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

/// `latest` means the newest session. When a live isolated run holds that one,
/// silently resolving to the second newest would spend an account window on a
/// conversation the operator never named — and `clauth sessions` would still be
/// listing the one they meant, first.
#[test]
fn latest_refuses_rather_than_substituting_when_an_isolated_run_holds_the_newest() {
    let sb = HomeSandbox::new();
    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let global = sb.home().join(".claude/projects/-w-g/sglobal.jsonl");
    write_jsonl(
        &global,
        &[user_line("sglobal", &ws.to_string_lossy(), "hi")],
    );
    set_mtime(&global, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));

    // Isolated transcript NEWER than the reachable one.
    let lock_file = live_isolated_session(&sb, "iso", "siso", &ws.to_string_lossy(), 5_000);
    let err = run_resume("latest", None).expect_err("a shadowed `latest` must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("'siso'") && msg.contains("profile 'iso'"),
        "the refusal names the session that shadowed `latest`: {msg}"
    );
    assert!(
        !msg.contains("sglobal"),
        "and never quietly offers the second newest: {msg}"
    );

    // Same fixture, isolated transcript OLDER: `latest` is the reachable one and
    // resolves normally. Proves the refusal is the shadowing, not the presence
    // of any live isolated run at all.
    set_mtime(
        &sb.home()
            .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/siso.jsonl"),
        SystemTime::UNIX_EPOCH + Duration::from_secs(500),
    );
    let resolved = resolve_session("latest");
    drop(lock_file);
    assert!(
        matches!(resolved, Resolved::Ready(ref s) if s.id == "sglobal"),
        "an older isolated transcript shadows nothing"
    );
}

/// The shadowing check ranges over the same kind of file `latest` itself does.
/// A nested transcript in a live isolated store can be newer than everything and
/// is still never anybody's `latest`, so refusing on one would strand the resume
/// waiting for a rescue that changes nothing. Named exactly, it is still held —
/// the listing shows it, so "no session found" would be false.
#[test]
fn a_nested_isolated_transcript_does_not_shadow_latest() {
    let sb = HomeSandbox::new();
    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let global = sb.home().join(".claude/projects/-w-g/sglobal.jsonl");
    write_jsonl(
        &global,
        &[user_line("sglobal", &ws.to_string_lossy(), "hi")],
    );
    set_mtime(&global, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));

    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso");
    let nested = iso.join("siso/subagents/agent-abc.jsonl");
    write_jsonl(
        &nested,
        &[user_line("siso", &ws.to_string_lossy(), "nested")],
    );
    set_mtime(&nested, SystemTime::UNIX_EPOCH + Duration::from_secs(9_000));
    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held so the runtime reads as live

    let resolved = resolve_session("latest");
    let by_name = resolve_session("agent-abc");
    // Control: the same store's TOP-LEVEL transcript at that mtime does shadow,
    // so the pass above is the nesting and not an isolated run reading as dead.
    let top = iso.join("siso.jsonl");
    write_jsonl(&top, &[user_line("siso", &ws.to_string_lossy(), "top")]);
    set_mtime(&top, SystemTime::UNIX_EPOCH + Duration::from_secs(9_000));
    let shadowed = resolve_session("latest");
    drop(lock_file);

    assert!(
        matches!(resolved, Resolved::Ready(ref s) if s.id == "sglobal"),
        "a nested isolated transcript shadows nothing"
    );
    assert!(
        matches!(by_name, Resolved::Held(ref h) if h.session.id == "agent-abc"),
        "but naming it exactly still reports the run holding it"
    );
    assert!(
        matches!(shadowed, Resolved::Held(_)),
        "the same store's top-level transcript still shadows"
    );
}

// ── clauth info ──

#[test]
fn info_prints_the_resume_command_workspace_and_storage() {
    let sb = HomeSandbox::new();
    // Componentwise: `storage:` prints the index walk's own `DirEntry::path`,
    // which is natively separated. A one-shot `join(".claude/projects/…")`
    // stores its `/` verbatim and mismatches that spelling on windows.
    let path = sb
        .home()
        .join(".claude")
        .join("projects")
        .join("-w")
        .join("known-session.jsonl");
    write_jsonl(&path, &[user_line("known-session", "/ws/a", "hi")]);

    let Resolved::Ready(session) = resolve_session("known-session") else {
        panic!("the id must resolve");
    };
    assert_eq!(
        info_lines(&session, None),
        format!(
            "resume:    clauth resume known-session\nworkspace: /ws/a\nstorage:   {}",
            path.display()
        )
    );
}

/// A held session is reportable where it is not resumable: `info` launches
/// nothing, and its storage path is the only place any surface says where the
/// transcript actually lives. What it must NOT print is a resume command that
/// Claude Code would refuse.
#[test]
fn info_reports_a_held_session_without_offering_a_resume_command() {
    let sb = HomeSandbox::new();
    let lock_file = live_isolated_session(&sb, "iso", "siso", "/w/iso", 5_000);
    let resolved = resolve_session("siso");
    drop(lock_file);

    let Resolved::Held(hold) = resolved else {
        panic!("a live isolated run must resolve as held");
    };
    let lines = info_lines(&hold.session, Some(&hold.profile));
    assert!(
        !lines.contains("clauth resume"),
        "no resume command for a session a resume can't reach: {lines}"
    );
    assert!(
        lines.contains("under 'iso'"),
        "the holding profile is named: {lines}"
    );
    assert!(
        lines.contains("workspace: /w/iso"),
        "the recorded workspace still comes from the transcript: {lines}"
    );
    assert!(
        lines.contains("runtime-isolated"),
        "and the storage path points into the isolated store: {lines}"
    );
}

// ── resume <unknown id> ──

#[test]
fn resume_unknown_id_errors_naming_it_at_exit_one() {
    let sb = HomeSandbox::new();
    // A real session so the index isn't empty — the error must be "unknown id",
    // not "no sessions".
    let path = sb.home().join(".claude/projects/-w/known-session.jsonl");
    write_jsonl(&path, &[user_line("known-session", "/ws", "hi")]);

    let err = run_resume("ghost-id", None).expect_err("unknown id must error");
    assert!(
        err.to_string().contains("ghost-id"),
        "the error must name the unknown id: {err}"
    );
    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "an unknown id is a runtime error, not a usage error"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

// ── resume refuses a disabled target ─────────────────────────────────────
//
// `run_resume` never spawns directly — it always funnels through
// `crate::start::run`, whose first line is the authoritative
// "never a live session for a disabled account" gate (mirrors
// `cli.rs::disabled_target_refusal`'s `cmd_start` regression test).

/// Seed `enabled` as plain profiles and `disabled` as disabled ones, all
/// under one `AppConfig` so each `create_blank_profile` call's
/// `save_app_state` persists the growing name list instead of a fresh call
/// clobbering an earlier profile's entry.
fn seed_profiles(enabled: &[&str], disabled: &[&str]) {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    for name in enabled.iter().chain(disabled.iter()) {
        crate::actions::create_blank_profile(&mut config, (*name).to_string(), None, None, None)
            .expect("create profile");
    }
    for name in disabled {
        crate::actions::disable_profile(&mut config, name).expect("disable profile");
    }
}

#[test]
fn resume_refuses_an_explicit_disabled_profile_before_any_spawn() {
    let sb = HomeSandbox::new();
    seed_profiles(&[], &["off"]);

    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let ws_str = ws.to_string_lossy().into_owned();
    let path = sb.home().join(".claude/projects/-w/known-session.jsonl");
    write_jsonl(&path, &[user_line("known-session", &ws_str, "hi")]);

    let err =
        run_resume("known-session", Some("off")).expect_err("a disabled target must be refused");
    assert_eq!(
        err.to_string(),
        "'off': account is disabled, run `clauth enable off`"
    );
    assert!(
        !sb.home().join(".clauth/profiles/off/runtime").exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

// ── resume_candidates: the interactive picker never offers a disabled account ──

#[test]
fn resume_candidates_excludes_disabled_accounts() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["acme"], &["off"]);
    let config = crate::profile::load_config().expect("reload");

    let (enabled, _) = resume_candidates(&config, "acme");

    assert_eq!(
        enabled,
        vec!["acme"],
        "a disabled account must never be an offered candidate"
    );
}

#[test]
fn resume_candidates_falls_back_when_the_default_is_disabled() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["acme"], &["off"]);
    let config = crate::profile::load_config().expect("reload");

    // A stale last-ran profile that's since been disabled must not be shown
    // as the bracketed default for a name that isn't even in the list.
    let (enabled, default) = resume_candidates(&config, "off");

    assert_eq!(enabled, vec!["acme"]);
    assert_eq!(
        default, "acme",
        "a disabled default must fall back to an enabled name"
    );
}
