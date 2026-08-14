#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! Guard coverage for `delegate`'s two new inputs: `prompt_file` (a reusable
//! prompt read from disk, validated against the delegate's `cwd`) and
//! `profiles` (a background-only fan-out that spends one window per account).
//!
//! Every refusal here is pinned on the reason it names, so a guard dropped
//! during a later edit fails its test rather than silently passing.

use super::*;
use crate::profile::{AppConfig, AppState};
use crate::testutil::HomeSandbox;
use std::io::{Seek, SeekFrom, Write};

/// A `DelegateArgs` with every optional field unset and JSON format, so each
/// test overrides only what it exercises.
fn base() -> DelegateArgs {
    DelegateArgs {
        profile: None,
        profiles: None,
        prompt: None,
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
        monitor: None,
        format: Some("json".to_string()),
    }
}

/// Seed `names` on disk, optionally disabling each so a stray spawn refuses
/// before launching `claude`.
fn seed_profiles(names: &[&str], disabled: bool) {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    for name in names {
        crate::actions::create_blank_profile(&mut config, (*name).to_string(), None, None, None)
            .expect("create profile");
    }
    if disabled {
        for name in names {
            crate::actions::disable_profile(&mut config, name).expect("disable profile");
        }
    }
}

/// Drive the async `delegate` tool with `CLAUTH_MCP_DEPTH` cleared, so the
/// recursion guard does not mask the argument guard under test. Every caller
/// holds a `HomeSandbox`, whose `HOME_TEST_LOCK` serializes the env mutation.
///
/// # Safety
/// `remove_var`/`set_var` are unsafe in Rust 2024 (not thread-safe); the lock
/// held by the caller's `HomeSandbox` is the serialization. Restored before this
/// returns, so no other lock-holder observes a torn value.
fn call_delegate(args: DelegateArgs) -> CallToolResult {
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the sandbox's HOME_TEST_LOCK.
    unsafe { std::env::remove_var(MCP_DEPTH_ENV) };

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

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("first content block is text")
}

/// A JSON-format refusal: one block, `is_error`, and every needle in the reason.
fn assert_refusal(result: &CallToolResult, needles: &[&str]) {
    assert_eq!(result.is_error, Some(true), "the refusal is a tool error");
    assert_eq!(
        result.content.len(),
        1,
        "the refusal is a single content block"
    );
    let body: serde_json::Value =
        serde_json::from_str(&first_text(result)).expect("refusal is JSON");
    assert_eq!(body["is_error"], serde_json::Value::Bool(true));
    let reason = body["result"].as_str().expect("reason is a string");
    for needle in needles {
        assert!(
            reason.contains(needle),
            "the reason names {needle:?}: {reason}"
        );
    }
}

/// A prose-format refusal: one block, a sentence that is not JSON, naming every
/// needle.
fn assert_prose_refusal(result: &CallToolResult, needles: &[&str]) {
    assert_eq!(result.is_error, Some(true), "the refusal is a tool error");
    assert_eq!(
        result.content.len(),
        1,
        "the prose refusal is a single content block"
    );
    let text = first_text(result);
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "the prose refusal must not be JSON"
    );
    for needle in needles {
        assert!(text.contains(needle), "the prose names {needle:?}: {text}");
    }
}

/// Poll the sandbox jobs dir until `count` jobs reached `done`, holding the home
/// override alive so their `write_done` lands under the sandbox, never the real
/// `~/.clauth`.
fn wait_for_jobs_done(count: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let done = jobs::jobs_dir()
            .ok()
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let id = e.path().file_stem()?.to_str()?.to_string();
                        jobs::read(&id).map(|r| r.state == jobs::JobState::Done)
                    })
                    .filter(|done| *done)
                    .count()
            })
            .unwrap_or(0);
        if done >= count {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "jobs never reached done"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn work_dir(home: &std::path::Path) -> std::path::PathBuf {
    let dir = home.join("work");
    std::fs::create_dir_all(&dir).expect("create work dir");
    dir
}

// ── prompt source: exactly one of `prompt` / `prompt_file` ───────────────────

#[test]
fn both_prompt_sources_are_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        prompt: Some("hi".to_string()),
        prompt_file: Some("p.txt".to_string()),
        profile: Some("solo".to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &["exactly one of `prompt` or `prompt_file` must be given; both were"],
    );
}

#[test]
fn neither_prompt_source_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &["exactly one of `prompt` or `prompt_file` must be given; neither was"],
    );
}

// ── target: exactly one of `profile` / `profiles` ────────────────────────────

#[test]
fn both_targets_are_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        profiles: Some(vec!["vendor".to_string()]),
        prompt: Some("hi".to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &["exactly one of `profile` or `profiles` must be given; both were"],
    );
}

#[test]
fn neither_target_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        prompt: Some("hi".to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &["exactly one of `profile` or `profiles` must be given; neither was"],
    );
}

// ── prompt_file boundary validation ──────────────────────────────────────────

#[test]
fn prompt_file_absolute_path_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    // The sandbox home is already absolute, so this is absolute on every
    // platform. A literal like `/etc/passwd` is not: on Windows it has a root
    // but no drive prefix, so `is_absolute()` is false there and the path
    // falls through to a file-not-found instead of the refusal under test.
    let abs = std::path::absolute(home.home().join("passwd"))
        .expect("absolute path")
        .to_str()
        .expect("sandbox path is UTF-8")
        .to_string();
    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        prompt_file: Some(abs.clone()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(&result, &[&format!("prompt_file `{abs}`"), "absolute path"]);
}

/// On Windows `is_absolute()` needs BOTH a prefix and a root, so a
/// drive-relative (`C:foo`) and a root-relative (`\etc\passwd`) spelling pass
/// the check at the top of the join and arrive at the component loop. The
/// `RootDir | Prefix` arm must refuse each by name: dropping the component
/// re-roots the path under `cwd` and reads a different file than the caller
/// named.
#[cfg(windows)]
#[test]
fn prompt_file_drive_relative_path_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    for rel in ["C:foo", r"\etc\passwd"] {
        let result = call_delegate(DelegateArgs {
            profile: Some("solo".to_string()),
            prompt_file: Some(rel.to_string()),
            cwd: Some(cwd.to_str().unwrap().to_string()),
            ..base()
        });
        assert_refusal(&result, &[&format!("prompt_file `{rel}`"), "absolute path"]);
    }
}

#[test]
fn prompt_file_dotdot_escape_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        prompt_file: Some("../secret.txt".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(&result, &["prompt_file `../secret.txt`", "escapes `cwd`"]);
}

#[cfg(unix)]
#[test]
fn prompt_file_symlink_escape_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    let outside = home.home().join("secret.txt");
    std::fs::write(&outside, "secret").expect("outside file");
    std::os::unix::fs::symlink(&outside, cwd.join("link.txt")).expect("symlink");

    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        prompt_file: Some("link.txt".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "prompt_file `link.txt`",
            "symlink target resolves outside `cwd`",
        ],
    );
}

#[test]
fn prompt_file_oversize_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    std::fs::write(
        cwd.join("big.txt"),
        vec![b'a'; super::PROMPT_FILE_CAP as usize + 1],
    )
    .expect("oversize file");

    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        prompt_file: Some("big.txt".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &["prompt_file `big.txt`", "bytes over the", "byte cap"],
    );
}

/// A directory used to end in an EISDIR-shaped refusal at read time. The type
/// check refuses it deliberately, by name, before any open.
#[test]
fn prompt_file_directory_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());

    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        prompt_file: Some(".".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(&result, &["prompt_file `.`", "not a regular file"]);
}

/// A FIFO blocks a read-only open until a writer appears, and the MCP server
/// runs on the only thread of its current-thread runtime, so reading one as a
/// `prompt_file` would freeze every tool until the process dies. The type check
/// must refuse it without ever opening it. On a regression the call below hangs
/// forever; the receive timeout turns that hang into a failing test instead of
/// a wedged runner.
#[cfg(unix)]
#[test]
fn prompt_file_refuses_a_fifo_without_blocking() {
    let home = HomeSandbox::new();
    let cwd = work_dir(home.home());
    let fifo = cwd.join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "mkfifo creates the fifo");

    let (tx, rx) = std::sync::mpsc::channel();
    let cwd_str = cwd.to_str().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        let _ = tx.send(super::read_prompt_file(Some(&cwd_str), "pipe"));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Err(reason)) => {
            assert!(
                reason.contains("not a regular file"),
                "the refusal names the file type: {reason}"
            );
        }
        Ok(Ok(_)) => panic!("a FIFO must never be read as a prompt"),
        Err(_) => panic!(
            "read_prompt_file blocked on a FIFO: the type check no longer refuses before the open"
        ),
    }
    handle.join().expect("reader thread joins");
}

/// A file grown past the cap after its size was checked must be refused by the
/// bounded read, never silently truncated: `take(cap + 1)` alone returns a
/// short Ok that reads as success.
#[test]
fn prompt_handle_growth_past_cap_is_refused_by_name() {
    let home = HomeSandbox::new();
    let path = home.home().join("grow.txt");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .expect("create grow.txt");
    f.write_all(&vec![b'a'; super::PROMPT_FILE_CAP as usize])
        .expect("cap bytes");
    // Grow past the cap on the same handle: a size check statting this file
    // before the growth sees a passing size; the read then sees past-cap bytes.
    f.write_all(b"more").expect("grow");
    f.seek(SeekFrom::Start(0)).expect("rewind");

    let reason = super::read_prompt_handle(f, "grow.txt")
        .expect_err("a past-cap read is refused, not truncated");
    for needle in [
        "prompt_file `grow.txt`",
        "grew past the",
        "byte cap",
        "during the read",
    ] {
        assert!(
            reason.contains(needle),
            "the reason names {needle:?}: {reason}"
        );
    }
}

/// The cap itself stays accepted: the growth refusal fires only past it.
#[test]
fn prompt_handle_at_cap_is_accepted() {
    let home = HomeSandbox::new();
    let path = home.home().join("exact.txt");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .expect("create exact.txt");
    f.write_all(&vec![b'a'; super::PROMPT_FILE_CAP as usize])
        .expect("cap bytes");
    f.seek(SeekFrom::Start(0)).expect("rewind");

    let text = super::read_prompt_handle(f, "exact.txt").expect("at-cap file is accepted");
    assert_eq!(
        text.len(),
        super::PROMPT_FILE_CAP as usize,
        "the at-cap file is read whole, not truncated"
    );
}

// ── profiles fan-out guards ──────────────────────────────────────────────────

#[test]
fn profiles_empty_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec![]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &["`profiles` is empty", "name at least one profile"],
    );
}

#[test]
fn profiles_over_cap_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let names: Vec<String> = (0..=super::MAX_FANOUT).map(|i| format!("p{i}")).collect();
    let result = call_delegate(DelegateArgs {
        profiles: Some(names),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["fan-out capped at", "names; got"]);
}

#[test]
fn profiles_duplicate_is_refused_by_name() {
    let _home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "SOLO".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "duplicate profile in `profiles`: `SOLO`",
            "case-insensitive",
        ],
    );
}

#[test]
fn profiles_unknown_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["ghost".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["profile not found: ghost"]);
}

#[test]
fn profiles_blocking_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt: Some("hi".to_string()),
        background: None,
        ..base()
    });
    assert_refusal(&result, &["`profiles` requires `background: true`"]);
}

/// A reserve failure refuses before any spawn: with the jobs dir replaced by a
/// regular file the first job-file write fails (ENOTDIR), and the fan-out must
/// name that failure and launch nothing rather than spending one window per
/// account mid-loop and losing the job ids.
#[test]
fn fanout_reserve_failure_is_refused_by_name() {
    let home = HomeSandbox::new();
    // Enabled members: a disabled one would refuse at the pre-flight before
    // the reserve this test pins.
    seed_profiles(&["solo", "vendor"], false);
    let jobs = home.home().join(".clauth").join("jobs");
    std::fs::create_dir_all(jobs.parent().unwrap()).expect("clauth dir");
    std::fs::write(&jobs, b"not a dir").expect("jobs path is a file");

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["failed to record job"]);
}

// ── background pre-flight guards ─────────────────────────────────────────────

/// Seed `name` as a keyless third-party profile: a real DeepSeek endpoint with
/// no api key, so the pre-flight refuses it before any job is reserved.
fn seed_keyless_third_party(name: &str) {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        name.to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");
}

/// The refusal envelope carries no job handle: no `job_id` key on a
/// single-target refusal, no `jobs` key on a fan-out refusal.
fn assert_no_job_keys(result: &CallToolResult) {
    let body: serde_json::Value =
        serde_json::from_str(&first_text(result)).expect("refusal is JSON");
    assert!(body.get("job_id").is_none(), "no job_id in the refusal");
    assert!(body.get("jobs").is_none(), "no jobs key in the refusal");
}

/// Nothing was reserved: the sandbox jobs dir is absent or empty.
fn assert_no_job_files() {
    // `HomeSandbox` holds the home override for the caller's whole body, so a
    // resolution failure here is a harness break, not an absent reservation.
    let dir = jobs::jobs_dir().expect("jobs dir resolvable");
    if !dir.exists() {
        return;
    }
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("jobs dir readable")
        .flatten()
        .collect();
    assert!(
        entries.is_empty(),
        "a refused delegate reserves no job file"
    );
}

/// A background single delegate to a keyless third-party profile refuses
/// synchronously, before a job file exists: the caller must not get a
/// `running` job whose collected result later carries the refusal.
#[test]
fn background_single_keyless_third_party_refuses_before_reserving_a_job() {
    let _home = HomeSandbox::new();
    seed_keyless_third_party("zzbg-ds");

    let result = call_delegate(DelegateArgs {
        profile: Some("zzbg-ds".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["profile has no api key: zzbg-ds"]);
    assert_no_job_keys(&result);
    assert_no_job_files();
}

/// The disabled sibling: a background single delegate to a disabled profile
/// refuses synchronously too, before a job file exists.
#[test]
fn background_single_disabled_target_refuses_before_reserving_a_job() {
    let _home = HomeSandbox::new();
    seed_profiles(&["zzbg-off"], true);

    let result = call_delegate(DelegateArgs {
        profile: Some("zzbg-off".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["profile is disabled: zzbg-off"]);
    assert_no_job_keys(&result);
    assert_no_job_files();
}

/// A disabled fan-out member refuses the whole list synchronously, by name,
/// before the first job file is reserved. Same pre-flight as the
/// single-background arm, closing the fan-out's disabled gap.
#[test]
fn background_fanout_with_a_disabled_member_refuses_before_writing_jobs() {
    let _home = HomeSandbox::new();
    // One config for both members: `load_config` reads the roster from the app
    // state, so a second fresh config would overwrite the first member.
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "zzbg-off".to_string(), None, None, None)
        .expect("create profile");
    crate::actions::disable_profile(&mut config, "zzbg-off").expect("disable profile");
    crate::actions::create_blank_profile(
        &mut config,
        "zzbg-ds".to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");

    // The disabled member comes FIRST: the pre-flight walks members in order,
    // so the refusal names it, not the keyless member behind it.
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["zzbg-off".to_string(), "zzbg-ds".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["profile is disabled: zzbg-off"]);
    assert_no_job_keys(&result);
    assert_no_job_files();
}

// ── happy path + format honouring ────────────────────────────────────────────

#[test]
fn a_valid_fanout_returns_one_job_per_account() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], false);

    // The members are enabled (a disabled member now refuses the fan-out at
    // the pre-flight); a nonexistent cwd stops each detached task at the cwd
    // gate so no stray claude spawns on the blank enabled profiles.
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "VENDOR".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_ne!(
        result.is_error,
        Some(true),
        "a valid fan-out is not an error"
    );
    assert_eq!(
        result.content.len(),
        1,
        "the fan-out reply is a single content block"
    );
    let body: serde_json::Value =
        serde_json::from_str(&first_text(&result)).expect("fan-out reply is JSON");
    let jobs = body["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 2, "one job per named account");

    let mut profiles: Vec<&str> = jobs
        .iter()
        .map(|j| j["profile"].as_str().expect("profile"))
        .collect();
    profiles.sort_unstable();
    assert_eq!(
        profiles,
        vec!["solo", "vendor"],
        "resolved target list echoed, wrong case canonicalised"
    );

    let ids: Vec<&str> = jobs
        .iter()
        .map(|j| j["job_id"].as_str().expect("job_id"))
        .collect();
    assert_eq!(ids.len(), 2, "one job id per account");
    assert_ne!(ids[0], ids[1], "job ids are distinct");

    // Hold the sandbox until both detached tasks finish, so their `write_done`
    // lands under the sandbox and never the real `~/.clauth`.
    wait_for_jobs_done(2);
}

#[test]
fn prose_refusals_read_as_a_sentence_and_stay_one_block() {
    let _home = HomeSandbox::new();

    let both = call_delegate(DelegateArgs {
        prompt: Some("hi".to_string()),
        prompt_file: Some("p.txt".to_string()),
        profile: Some("solo".to_string()),
        format: None,
        ..base()
    });
    assert_prose_refusal(
        &both,
        &["delegate failed: exactly one of `prompt` or `prompt_file` must be given; both were"],
    );

    let blocking = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt: Some("hi".to_string()),
        background: None,
        format: None,
        ..base()
    });
    assert_prose_refusal(
        &blocking,
        &["delegate failed: `profiles` requires `background: true`"],
    );
}

#[test]
fn fanout_prose_names_each_target_with_its_job() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], false);

    // Enabled members plus a nonexistent cwd: same stray-spawn guard as the
    // JSON fan-out test above.
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        format: None,
        ..base()
    });
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        result.content.len(),
        1,
        "the prose fan-out is a single content block"
    );
    let text = first_text(&result);
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "the prose fan-out must not be JSON"
    );
    assert!(
        text.starts_with("delegated to "),
        "prose reads as a sentence: {text}"
    );
    assert!(
        text.contains("`solo` (job `"),
        "names each target with its job: {text}"
    );
    assert!(
        text.contains("`vendor` (job `"),
        "names each target with its job: {text}"
    );

    wait_for_jobs_done(2);
}
