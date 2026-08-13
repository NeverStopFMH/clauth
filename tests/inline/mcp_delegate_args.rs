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
    let result = call_delegate(DelegateArgs {
        profile: Some("solo".to_string()),
        prompt_file: Some("/etc/passwd".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(&result, &["prompt_file `/etc/passwd`", "absolute path"]);
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

// ── profiles fan-out guards ──────────────────────────────────────────────────

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

// ── happy path + format honouring ────────────────────────────────────────────

#[test]
fn a_valid_fanout_returns_one_job_per_account() {
    let _home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], true);

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "VENDOR".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
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
    let _home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], true);

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
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
