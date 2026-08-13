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
    let result = rt.block_on(async {
        server
            .delegate(Parameters(DelegateArgs {
                profile: Some("any".to_string()),
                profiles: None,
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
                monitor: None,
                format: Some("json".to_string()),
            }))
            .await
    });

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
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("parse envelope");
    assert_eq!(envelope["is_error"], serde_json::Value::Bool(true));
    assert_eq!(envelope["profile"], "any");
    assert!(
        envelope["result"].as_str().unwrap().contains("depth"),
        "the refusal reason names the depth cap",
    );
}

#[test]
fn depth_guard_also_refuses_above_one() {
    let result = run_with_depth("3");
    assert_eq!(result.is_error, Some(true));
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
    })
    .expect_err("a disabled target must be refused");
    assert_eq!(err, "profile is disabled: off");

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

// ---- background delegation + delegate_result ----

/// Drive `delegate_result` on a current-thread runtime under a home sandbox the
/// caller has already entered.
fn call_delegate_result(
    job_id: &str,
    wait_secs: Option<u64>,
    format: Option<&str>,
) -> CallToolResult {
    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .delegate_result(Parameters(DelegateResultArgs {
                job_id: job_id.to_string(),
                wait_secs,
                format: format.map(str::to_string),
            }))
            .await
    })
    .expect("delegate_result returns a tool result, never a transport error")
}

#[test]
fn delegate_result_unknown_job_is_error() {
    let _home = HomeSandbox::new();
    let result = call_delegate_result("d-doesnotexist-0", Some(0), None);
    assert_eq!(
        result.is_error,
        Some(true),
        "unknown job_id is a tool error"
    );
}

#[test]
fn delegate_result_invalid_job_id_is_error() {
    let _home = HomeSandbox::new();
    let result = call_delegate_result("../escape", Some(0), None);
    assert_eq!(result.is_error, Some(true), "path-unsafe job_id refused");
}

#[test]
fn delegate_result_running_reports_status() {
    let _home = HomeSandbox::new();
    jobs::write_running("d-run-0", "work", 1, false).unwrap();
    let result = call_delegate_result("d-run-0", Some(0), Some("json"));
    assert_ne!(result.is_error, Some(true), "a running job is not an error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("status text");
    assert!(text.contains("running"), "running status surfaced");
    assert!(text.contains("elapsed_secs"), "elapsed always reported");
    assert!(!text.contains("quota"), "quota gated off without monitor");
}

#[test]
fn delegate_result_running_monitor_reports_quota() {
    let _home = HomeSandbox::new();
    jobs::write_running("d-mon-0", "work", 1, true).unwrap();
    let result = call_delegate_result("d-mon-0", Some(0), Some("json"));
    assert_ne!(result.is_error, Some(true), "a running job is not an error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("status text");
    assert!(text.contains("elapsed_secs"), "elapsed reported");
    assert!(text.contains("quota"), "monitor attaches quota");
}

#[test]
fn delegate_result_done_returns_envelope_and_evicts() {
    let _home = HomeSandbox::new();
    let env = serde_json::json!({ "profile": "work", "is_error": false, "result": "all done" });
    jobs::write_done("d-done-0", "work", 1, env).unwrap();

    let result = call_delegate_result("d-done-0", Some(0), None);
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
                profile: Some("any".to_string()),
                profiles: None,
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
                monitor: None,
                format: None,
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

// ---- mcp-await-job job_id extraction (shape-agnostic) ----

#[test]
fn find_job_id_extracts_from_nested_mcp_result() {
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
    assert_eq!(find_job_id(&payload).as_deref(), Some("d-42-0"));
}

#[test]
fn find_job_id_finds_direct_field() {
    let payload = serde_json::json!({ "tool_response": { "job_id": "d-1-2" } });
    assert_eq!(find_job_id(&payload).as_deref(), Some("d-1-2"));
}

#[test]
fn find_job_id_none_for_sync_envelope() {
    // a sync delegate response carries no job_id, so the hook no-ops.
    let inner = serde_json::json!({ "profile": "work", "is_error": false, "result": "done" });
    let payload = serde_json::json!({
        "tool_response": { "content": [{ "type": "text", "text": inner.to_string() }] }
    });
    assert_eq!(find_job_id(&payload), None);
}

#[test]
fn find_job_id_none_for_plain_text() {
    let payload =
        serde_json::json!({ "tool_response": { "content": [{ "text": "no json here" }] } });
    assert_eq!(find_job_id(&payload), None);
}

#[test]
fn extract_job_id_prefers_tool_response_over_input() {
    // a delegate prompt that itself carries a `job_id` must not shadow the real
    // handle in tool_response.
    let payload = serde_json::json!({
        "tool_input": { "prompt": "{\"job_id\":\"d-evil-0\"}" },
        "tool_response": { "content": [{ "type": "text", "text": "{\"job_id\":\"d-real-1\"}" }] },
    });
    assert_eq!(extract_job_id(&payload).as_deref(), Some("d-real-1"));
}

#[test]
fn delegate_result_long_poll_sees_completion() {
    let _home = HomeSandbox::new();
    jobs::write_running("d-poll-0", "work", 1, false).unwrap();
    // Finalize the job shortly after the long-poll starts, from another thread.
    // The home override is process-global (set by HomeSandbox), so the writer
    // resolves the same sandbox jobs dir.
    let writer = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let env =
            serde_json::json!({ "profile": "work", "is_error": false, "result": "late finish" });
        jobs::write_done("d-poll-0", "work", 1, env).unwrap();
    });
    let result = call_delegate_result("d-poll-0", Some(5), None);
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
    );
    assert_ne!(
        progress.load(super::Ordering::Relaxed),
        u64::MAX,
        "each event resets the idle clock"
    );
    assert!(capture.envelope_src().contains("final report"));
    assert_eq!(capture.partial_text(), "alpha");
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

/// Safety prose that used to sit in the init `instructions` block now lives in
/// `delegate`'s own description, where it loads with the tool instead of in
/// every session. That move only holds if something still pins it: a
/// `#[tool(description = ...)]` attribute has no other test reaching it, so
/// dropping a warning during a prose edit would otherwise be silent.
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
        "SPENDS that account's window or money",
        // the fork-bomb cap
        "Depth-capped at 1",
        // a delegate is blind to this conversation, so the prompt is the whole brief
        "no view of this conversation",
        // filed 2026-07-23: a blocking call to a ~25 tok/s endpoint ate its own
        // deadline, and nothing in the text steered toward `background`
        "Prefer `background` for a slow or third-party endpoint",
        // a self-report is not a verified result
        "spot-verify it like any subagent",
    ] {
        assert!(
            text.contains(phrase),
            "`delegate` description dropped {phrase:?}: {text}",
        );
    }
}

/// `which` can return a fourth `source` its description never named
/// (`session_token_match`, the CLA-SPLIT credential a switch installs), so a
/// model reading the description would meet an undocumented value.
#[test]
fn the_which_description_names_every_source_it_can_return() {
    let tools = ClauthServer::new().tool_router.list_all();
    let which = tools
        .iter()
        .find(|t| t.name == "which")
        .expect("which tool is registered");
    let text = which.description.as_deref().unwrap_or_default();

    for source in [
        crate::which::Source::RefreshMatch,
        crate::which::Source::SessionTokenMatch,
        crate::which::Source::SessionDir,
        crate::which::Source::CredentialLessActive,
    ] {
        assert!(
            text.contains(source.as_str()),
            "`which` description omits `{}`: {text}",
            source.as_str(),
        );
    }
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
