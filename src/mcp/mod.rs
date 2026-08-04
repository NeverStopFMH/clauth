//! `clauth mcp` — MCP JSON-RPC 2.0 server over stdio (rmcp).
//!
//! Exposes clauth profiles to a live Claude Code session: list/usage, switch,
//! and delegate. The rest of the binary stays synchronous; [`serve`] builds a
//! scoped current-thread tokio runtime and blocks on the stdio server.
//!
//! All logging MUST go to stderr — stdout carries the JSON-RPC frame.

mod jobs;
mod render;

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::logline::logline;
use crate::profile::{AppConfig, load_config};
use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache};
use crate::profile_json::{provider_label, tier_label, windows_json};
use crate::providers::ThirdPartyStats;
use crate::runtime::{Isolation, ProfileRuntime};
use crate::usage::{UsageInfo, UsageWindow, now_epoch_secs, now_ms};
use render::ProfileSnapshot;

/// Marks the `clauth mcp` child that [`crate::plugin_probe::mcp_boots`] spawns
/// for the Plugin tab's handshake check. clauth owns both sides of that spawn, so
/// an env marker beats inferring it from the client's `initialize`.
pub(crate) const MCP_PROBE_ENV: &str = "CLAUTH_MCP_PROBE";

/// Default wall-clock ceiling (seconds) on one delegate. A wall clock cannot see
/// whether the child is producing anything, so it kills a delegate mid-answer
/// exactly like a hung one; it is the backstop and [`DEFAULT_IDLE_SECS`] is what
/// normally ends a stuck run.
const DEFAULT_RUN_TIMEOUT_SECS: u64 = MAX_RUN_TIMEOUT_SECS;
/// Hard ceiling on a caller-supplied delegate timeout (seconds).
const MAX_RUN_TIMEOUT_SECS: u64 = 3600;
/// Default idle deadline (seconds): kill only once the delegate has emitted
/// NOTHING for this long. Every streamed event resets it, so a working delegate
/// runs to the wall clock no matter how long it takes. It must stay above the
/// longest single blocking tool call a delegate makes (a release build), since
/// no event arrives while one runs.
const DEFAULT_IDLE_SECS: u64 = 300;
/// Cap on the salvaged assistant text carried back by a killed delegate. The
/// tail is kept: it is the part closest to a usable answer.
const PARTIAL_TEXT_CAP: usize = 8 * 1024;
/// Raise the delegate's max output budget above CC's default so a long headless
/// build doesn't die on the 32k cap. Overridable via the `env` arg.
const DEFAULT_MAX_OUTPUT_TOKENS: &str = "64000";

/// Compact per-model throughput rows for a profile (observed tok/s, degraded /
/// rate-limited flags). Empty array when clauth has launched no runs for it.
fn throughput_json(profile: &str, now: i64) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = crate::throughput::summary(profile, now)
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "model": m.model,
                "tok_s": (m.tok_s * 10.0).round() / 10.0,
                "samples": m.samples,
                "degraded": m.degraded,
                "rate_limited_recent": m.rate_limited_recent,
                "retry_after_s": m.retry_after_s,
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

/// Fresh-from-cache 5h/7d windows for a profile. Each call re-reads the disk
/// cache (no caching across tool calls per the design).
fn load_windows(name: &str) -> (Option<UsageWindow>, Option<UsageWindow>) {
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(u) => (u.five_hour, u.seven_day),
        None => (None, None),
    }
}

/// Live footer for the current active profile, read fresh from cache.
fn active_footer(config: &AppConfig) -> String {
    let active = config.state.active_profile.as_deref();
    let (five_h, seven_d) = match active {
        Some(name) => load_windows(name),
        None => (None, None),
    };
    render::live_footer(active, five_h.as_ref(), seven_d.as_ref())
}

/// Append the live footer to a JSON text payload as a second content block.
fn with_footer(json: serde_json::Value, footer: String) -> Vec<ContentBlock> {
    vec![
        ContentBlock::text(json.to_string()),
        ContentBlock::text(footer),
    ]
}

#[derive(Clone)]
pub(crate) struct ClauthServer {
    // consumed by the `#[tool_handler]` macro at dispatch time; rustc's
    // dead-code pass can't see through the macro plumbing.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SwitchArgs {
    /// Profile name to relink the global active credentials to.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateArgs {
    /// Profile name to run the headless delegate session under.
    profile: String,
    /// Prompt passed to the delegated `claude -p` session.
    prompt: String,
    /// Optional model override for the delegated session.
    model: Option<String>,
    /// Working directory for the delegate (must exist). Defaults to the MCP
    /// server's cwd. Set a clean dir to keep the delegate from picking up a
    /// project `CLAUDE.md`.
    cwd: Option<String>,
    /// Extra environment variables for the delegate (e.g.
    /// `CLAUDE_CODE_MAX_OUTPUT_TOKENS`). `CLAUDE_CONFIG_DIR` and the depth guard
    /// are always set by clauth and cannot be overridden here.
    env: Option<HashMap<String, String>>,
    /// Extra arguments appended to the `claude` invocation (after clauth's own
    /// `-p` and streaming flags, the isolated-only `--strict-mcp-config`, and
    /// `--model <model>` when `model` is set). Pinning `--output-format` here
    /// replaces clauth's, which also turns the idle deadline off.
    args: Option<Vec<String>>,
    /// Wall-clock ceiling in seconds (1..=3600). Backstop only; `idle_secs` is
    /// what normally ends a stuck delegate. Defaults to 3600.
    timeout_secs: Option<u64>,
    /// Kill the delegate after this many seconds with NO output at all
    /// (1..=3600). Defaults to 300. A delegate that keeps streaming is never cut
    /// off, so raise this only when the task makes one blocking tool call longer
    /// than the default (a long build). Ignored when `args` pins its own
    /// `--output-format`, which turns the event stream off.
    idle_secs: Option<u64>,
    /// Continue an earlier delegate instead of starting fresh: the `session_id`
    /// a killed run handed back. `prompt` becomes the next turn of that
    /// conversation. clauth runs it in the workspace the session was recorded in,
    /// so `cwd` is unnecessary (and refused when it disagrees).
    resume: Option<String>,
    /// Run authenticated but without operator memory/plugins/hooks (a clean
    /// blind session). Defaults to false.
    isolated: Option<bool>,
    /// Return a `{job_id}` immediately instead of blocking for the result. The
    /// delegate runs on a detached task; collect the result via the auto-delivery
    /// hook or `delegate_result({job_id})`. Defaults to false.
    background: Option<bool>,
    /// Opt into progress reporting for a `background` run: a `delegate_result`
    /// poll on the still-running job then also reports the target profile's live
    /// usage windows (`quota`) alongside `elapsed_secs`. No effect on a blocking
    /// call. Defaults to false.
    monitor: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateResultArgs {
    /// Job id returned by a `delegate` call made with `background: true`.
    job_id: String,
    /// Seconds to long-poll for completion before returning (0..=60, default 0 =
    /// reply instantly with the current state).
    wait_secs: Option<u64>,
}

#[tool_router]
impl ClauthServer {
    pub(crate) fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List all clauth profiles from disk cache (zero quota). Per profile: \
`name`, `active` (is this the currently active profile), `provider` (`anthropic` or a recognised \
third-party name), and `base_url` (endpoint URL, null for a default OAuth profile) identify it; \
`tier` is the account plan label (e.g. `Max 5x`), null for a third-party/API-key profile or when \
no plan data is cached yet; a dead subscription reports the org's post-cancellation tier (`Free`), \
never the word `canceled` — cancellation is a status, not a tier; \
`windows[]` carries the 5h, 7d, and per-model weekly (`7d <model>`) `{label, utilization_pct, \
resets_at}` where `utilization_pct` is the percent of that window already USED (higher = less \
headroom) and `resets_at` is ISO-8601; \
`has_live_session` = a clauth-managed `claude` session currently owns it; `throughput[]` = \
observed per-model `{model, tok_s, samples, degraded, rate_limited_recent, retry_after_s}` from \
past `delegate` calls; \
`third_party` = a cached one-line headline for provider-key profiles (deepseek/zai/…)"
    )]
    async fn list_profiles(&self) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let now = now_epoch_secs();

        let profiles: Vec<serde_json::Value> = config
            .profiles
            .iter()
            .map(|p| {
                let name = p.name.as_str();
                let third_party = if p.is_third_party() {
                    load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                        .as_ref()
                        .map(render::third_party_headline)
                } else {
                    None
                };
                serde_json::json!({
                    "name": name,
                    "active": config.is_active(name),
                    "provider": provider_label(p),
                    "base_url": p.base_url,
                    "tier": tier_label(p),
                    "has_live_session": crate::runtime::has_live_session(name),
                    "windows": windows_json(name),
                    "third_party": third_party,
                    "throughput": throughput_json(name, now),
                })
            })
            .collect();

        let payload = serde_json::json!({ "profiles": profiles });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }

    #[tool(
        description = "Report which profile owns the credentials this session loaded. `source` \
explains how it resolved: `refresh_match` (a profile's stored token matches the live credentials), \
`session_dir` (this session's runtime dir pins the profile), `credential_less_active` (the \
configured active profile, with no credentials on disk to match). Appends a live usage footer (% used)"
    )]
    async fn which(&self) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let resolved = crate::which::resolve_active(&config);
        let throughput = resolved
            .as_ref()
            .map(|(name, _)| throughput_json(name, now_epoch_secs()))
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        let tier = resolved.as_ref().and_then(|(name, _)| {
            config
                .profiles
                .iter()
                .find(|p| p.name.as_str() == name.as_str())
                .and_then(tier_label)
        });
        let payload = serde_json::json!({
            "profile": resolved.as_ref().map(|(name, _)| name),
            "source": resolved.as_ref().map(|(_, source)| source.as_str()),
            "tier": tier,
            "throughput": throughput,
        });
        Ok(CallToolResult::success(with_footer(
            payload,
            active_footer(&config),
        )))
    }

    #[tool(
        description = "Relink the global active profile (`~/.claude` credentials). A `clauth start` session is pinned to its own runtime and unaffected; a session on the global credentials adopts the change on its next token refresh"
    )]
    async fn switch(
        &self,
        Parameters(SwitchArgs { name }): Parameters<SwitchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Resolve the raw tool argument to a stored profile (case-insensitive)
        // BEFORE any mutation — the same guard the CLI applies. Skipping it lets an
        // unknown/wrong-case name reach `link_profile_credentials`, which strips the
        // live `.credentials.json` symlink and creates no replacement (it only errors
        // later at `finish_switch`), leaving the global session credential-less.
        let Some(name) = config.canonical_name(&name) else {
            let payload =
                serde_json::json!({ "ok": false, "reason": format!("profile not found: {name}") });
            return Ok(CallToolResult::error(with_footer(
                payload,
                active_footer(&config),
            )));
        };
        let on_divergence = config.state.default_divergence;

        // `switch_profile_noninteractive` can block on the macOS keychain deadline
        // (up to 20s) and may refresh the target over HTTP (its AUTH-1 gate);
        // keep it off the async worker so neither stalls the runtime. Mirrors
        // `delegate`'s `spawn_blocking`. The shared-handle wrap is what the
        // gate's refresh path requires (it must lock/unlock around HTTP).
        let (config, outcome) = tokio::task::spawn_blocking(move || {
            let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
            let outcome = crate::actions::switch_profile_noninteractive(
                &config,
                &name,
                on_divergence,
                crate::oauth::refresh_result,
            );
            (config, outcome)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("switch task failed: {e}"), None))?;
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let config = config.lock().expect("config mutex poisoned");

        match outcome {
            Ok((previous, active)) => {
                let payload = serde_json::json!({
                    "ok": true,
                    "previous": previous,
                    "active": active,
                });
                Ok(CallToolResult::success(with_footer(
                    payload,
                    active_footer(&config),
                )))
            }
            Err(e) => {
                let payload = serde_json::json!({ "ok": false, "reason": e.to_string() });
                Ok(CallToolResult::error(with_footer(
                    payload,
                    active_footer(&config),
                )))
            }
        }
    }

    #[tool(
        description = "Delegate a headless task to a profile; SPENDS that account's real usage \
window. The depth-1 cap blocks only a nested clauth `delegate` (a delegate cannot delegate again); \
in-delegate subagents run, but under the SAME delegated profile, not other accounts. For a \
one-shot task, prefer `isolated: true`: a clean blind session that skips the operator persona \
(the runtime's `CLAUDE.md`, plugins, hooks, skills) and loads no MCP servers, so it is cheaper \
(and on an API-key profile, fewer billed tokens). A shared delegate (the default) instead inherits \
that persona plus the runtime config-dir's MCP servers; use it only when the task needs repo tools \
/ codebase nav. Scope a shared delegate with `args:[\"--mcp-config\",\"<json|path>\",\"--strict-mcp-config\"]`. \
Separately, a delegate loads the project `CLAUDE.md` of its `cwd` (defaults to this server's cwd) \
regardless of `isolated`, so set `cwd` to a clean dir for a one-shot to avoid an unrelated \
project's house-style. Optional cwd/env/args/timeout_secs/idle_secs/isolated shape the spawned \
`claude`. A delegate is killed only once it has emitted nothing for `idle_secs` (default 300) or \
hits the `timeout_secs` wall clock (default 3600), so a run that keeps working is never cut off; \
raise `idle_secs` when the task makes one long blocking call. A killed delegate returns \
`timed_out` plus whatever text it had produced in `partial_result` (its window is spent either \
way), and a `session_id` when the run can be picked back up: pass it as `resume` with a new \
`prompt` to continue that conversation instead of paying for the work again. An `isolated` run is \
resumable only with clauth's auto-rescue on, and the killed envelope says which case it is. \
Returns the delegate envelope (`result`, \
`is_error`, `total_cost_usd`, token usage): read `total_cost_usd`/usage to self-throttle; the \
`result` is the delegate's own self-report, so spot-verify it like any subagent. Set \
`background: true` to get a `{job_id}` back at once instead of blocking; the result auto-arrives \
via a hook, or fetch it with `delegate_result({job_id})`. Add `monitor: true` so a \
`delegate_result` poll on the still-running job reports `elapsed_secs` + the target's live `quota`"
    )]
    async fn delegate(
        &self,
        Parameters(DelegateArgs {
            profile,
            prompt,
            model,
            cwd,
            env,
            args,
            timeout_secs,
            idle_secs,
            resume,
            isolated,
            background,
            monitor,
        }): Parameters<DelegateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Fail closed: a present-but-unparseable value is treated as max depth
        // (refuse), so a corrupt env can never re-enable delegation. Only a truly
        // absent var is depth 0.
        let depth: u32 = match std::env::var(MCP_DEPTH_ENV) {
            Ok(v) => v.trim().parse().unwrap_or(u32::MAX),
            Err(_) => 0,
        };
        if depth >= 1 {
            let payload = serde_json::json!({
                "profile": profile,
                "is_error": true,
                "result": "delegation depth exceeded (max 1)",
            });
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }

        // Both deadlines resolve inside `run_delegate`: the wall clock's fallback
        // depends on whether the child ends up streaming, which only the composed
        // arg list knows.
        let isolation = if isolated.unwrap_or(false) {
            Isolation::Isolated
        } else {
            Isolation::Shared
        };

        // Background: persist a `running` job file, run the delegate on a detached
        // blocking task that finalizes the file on completion, and return the
        // handle now. The detached task outlives this call (it runs on the
        // blocking pool, not this turn's future) so N delegates overlap.
        if background.unwrap_or(false) {
            let started_at = now_ms();
            let job_id = jobs::new_job_id(started_at);
            jobs::write_running(&job_id, &profile, started_at, monitor.unwrap_or(false)).map_err(
                |e| ErrorData::internal_error(format!("failed to record job: {e}"), None),
            )?;

            let job_id_task = job_id.clone();
            let profile_task = profile.clone();
            tokio::task::spawn_blocking(move || {
                // Catch a panic in the detached task: the handle is dropped, so an
                // unwind would otherwise be swallowed and leave the job stuck
                // `running` until GC — the waiter would hang on its deadline. The
                // job file is always finalized, mirroring the sync contract.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_delegate(DelegateOpts {
                        profile: &profile_task,
                        prompt: &prompt,
                        model: model.as_deref(),
                        cwd: cwd.as_deref(),
                        env: env.unwrap_or_default(),
                        extra_args: args.unwrap_or_default(),
                        timeout_secs,
                        idle_secs,
                        resume: resume.as_deref(),
                        isolation,
                        depth,
                    })
                }));
                let envelope = match outcome {
                    Ok(Ok(v)) => v,
                    Ok(Err(reason)) => serde_json::json!({
                        "profile": profile_task,
                        "is_error": true,
                        "result": reason,
                    }),
                    Err(_) => serde_json::json!({
                        "profile": profile_task,
                        "is_error": true,
                        "result": "delegate task panicked",
                    }),
                };
                let _ = jobs::write_done(&job_id_task, &profile_task, started_at, envelope);
            });

            let payload = serde_json::json!({
                "job_id": job_id,
                "profile": profile,
                "started_at": started_at,
                "status": "running",
            });
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }

        let target = profile.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            run_delegate(DelegateOpts {
                profile: &target,
                prompt: &prompt,
                model: model.as_deref(),
                cwd: cwd.as_deref(),
                env: env.unwrap_or_default(),
                extra_args: args.unwrap_or_default(),
                timeout_secs,
                idle_secs,
                resume: resume.as_deref(),
                isolation,
                depth,
            })
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("delegate task panicked: {e}"), None))?;

        let envelope = match outcome {
            Ok(v) => v,
            Err(reason) => serde_json::json!({
                "profile": profile,
                "is_error": true,
                "result": reason,
            }),
        };

        let (five_h, seven_d) = load_windows(&profile);
        let mut footer =
            render::live_footer(Some(profile.as_str()), five_h.as_ref(), seven_d.as_ref());
        if let Some(note) = throughput_note(&profile, now_epoch_secs()) {
            footer.push('\n');
            footer.push_str(&note);
        }
        let is_error = envelope
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = with_footer(envelope, footer);
        if is_error {
            Ok(CallToolResult::error(content))
        } else {
            Ok(CallToolResult::success(content))
        }
    }

    #[tool(
        description = "Fetch the result of a `delegate` call made with `background: true`, by \
`job_id`. `wait_secs` (0..=60, default 0) long-polls for completion. Returns the delegate \
envelope when done (same shape as a blocking `delegate`, with the live usage footer), \
`{job_id, status:\"running\", elapsed_secs, quota?}` if it hasn't finished (`quota` present only \
when the job's `delegate` call set `monitor: true`), or an error for an unknown `job_id`. Normally \
the result auto-arrives via a hook. Use this only when delegate hooks are disabled"
    )]
    async fn delegate_result(
        &self,
        Parameters(DelegateResultArgs { job_id, wait_secs }): Parameters<DelegateResultArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if !jobs::is_safe_job_id(&job_id) {
            let payload = serde_json::json!({ "is_error": true, "result": "invalid job_id" });
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }
        let wait = wait_secs.unwrap_or(0).min(MAX_RESULT_WAIT_SECS);
        let jid = job_id.clone();
        let outcome = tokio::task::spawn_blocking(move || wait_for_done(&jid, wait))
            .await
            .map_err(|e| ErrorData::internal_error(format!("wait task panicked: {e}"), None))?;

        match outcome {
            WaitOutcome::Unknown => {
                let payload = serde_json::json!({ "is_error": true, "result": format!("unknown job_id: {job_id}") });
                Ok(CallToolResult::error(vec![ContentBlock::text(
                    payload.to_string(),
                )]))
            }
            WaitOutcome::Running(record) => {
                let elapsed_secs = now_ms().saturating_sub(record.started_at) / 1000;
                let mut payload = serde_json::json!({
                    "job_id": job_id,
                    "status": "running",
                    "elapsed_secs": elapsed_secs,
                });
                // `monitor`-gated: attach the target's live usage windows so the
                // poller sees remaining headroom without a separate list_profiles.
                if record.monitor {
                    payload["quota"] = windows_json(&record.profile);
                }
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    payload.to_string(),
                )]))
            }
            WaitOutcome::Done(record) => {
                // Fallback path delivered it — evict so the file doesn't linger
                // past its purpose (GC also reaps it on a TTL).
                jobs::remove(&job_id);
                let envelope = record.envelope.unwrap_or_else(|| {
                    serde_json::json!({
                        "profile": record.profile,
                        "is_error": true,
                        "result": "job finished without an envelope",
                    })
                });
                let (five_h, seven_d) = load_windows(&record.profile);
                let mut footer = render::live_footer(
                    Some(record.profile.as_str()),
                    five_h.as_ref(),
                    seven_d.as_ref(),
                );
                if let Some(note) = throughput_note(&record.profile, now_epoch_secs()) {
                    footer.push('\n');
                    footer.push_str(&note);
                }
                let is_error = envelope
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let content = with_footer(envelope, footer);
                if is_error {
                    Ok(CallToolResult::error(content))
                } else {
                    Ok(CallToolResult::success(content))
                }
            }
        }
    }
}

/// Env var carrying the MCP delegation depth; the child `claude` inherits
/// `depth+1` so a delegate cannot itself delegate (hard cap at 1).
const MCP_DEPTH_ENV: &str = "CLAUTH_MCP_DEPTH";

/// Poll interval mirroring `start.rs`'s `wait_for_child` cadence.
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Ceiling on `delegate_result`'s long-poll wait (seconds).
const MAX_RESULT_WAIT_SECS: u64 = 60;
/// Poll cadence for both `delegate_result` and the `mcp-await-job` hook.
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Self-deadline for the `mcp-await-job` hook: outlast the max delegate timeout
/// plus slack so it never gives up before a legitimately long delegate finishes.
const AWAIT_JOB_DEADLINE_SECS: u64 = MAX_RUN_TIMEOUT_SECS + 600;

/// Result of polling a background job file.
enum WaitOutcome {
    Done(jobs::JobRecord),
    /// Present but not yet finished (the wait deadline elapsed first). Carries the
    /// record so the caller can report `elapsed_secs` / monitored `quota`.
    Running(jobs::JobRecord),
    /// No such job file (never created or already evicted).
    Unknown,
}

/// Poll a job file until it reports `done` or `deadline_secs` elapses. `Unknown`
/// when the file is absent (distinct from `Running` for a present-but-incomplete
/// job). Blocking; callers wrap it in `spawn_blocking`.
fn wait_for_done(job_id: &str, deadline_secs: u64) -> WaitOutcome {
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    loop {
        match jobs::read(job_id) {
            Some(r) if r.state == jobs::JobState::Done => return WaitOutcome::Done(r),
            Some(r) if start.elapsed() >= deadline => return WaitOutcome::Running(r),
            Some(_) => {}
            None => return WaitOutcome::Unknown,
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
}

/// `clauth mcp-await-job` — the body of the bundled PostToolUse `asyncRewake`
/// hook. Reads the hook payload on stdin, finds the background job's `job_id`,
/// waits for the result, prints it to stdout, and exits 2 to wake the model. A
/// sync `delegate` (no `job_id` in the payload) is a no-op (exit 0). On its own
/// deadline it exits 2 with a nudge to call `delegate_result` instead.
pub(crate) fn await_job() -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let job_id = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .as_ref()
        .and_then(extract_job_id)
        .filter(|id| jobs::is_safe_job_id(id));
    let Some(job_id) = job_id else {
        std::process::exit(0); // sync delegate or unparseable input: nothing to deliver
    };

    let start = Instant::now();
    let deadline = Duration::from_secs(AWAIT_JOB_DEADLINE_SECS);
    loop {
        match jobs::read(&job_id) {
            Some(r) if r.state == jobs::JobState::Done => {
                let envelope = r.envelope.unwrap_or_else(|| {
                    serde_json::json!({
                        "profile": r.profile,
                        "is_error": true,
                        "result": "job finished without an envelope",
                    })
                });
                println!("{envelope}");
                std::process::exit(2); // wake the model with the result
            }
            Some(_) if start.elapsed() >= deadline => {
                println!(
                    "delegate job {job_id} still running; call `delegate_result` to retrieve it"
                );
                std::process::exit(2);
            }
            Some(_) => {}
            None => std::process::exit(0), // unknown / already evicted
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
}

/// Extract a background job's id from a hook payload, preferring the documented
/// `tool_response` slot so a delegate prompt that happens to carry a `job_id`
/// can't shadow the real handle; fall back to a whole-payload scan only if it's
/// absent (the exact shape is not host-guaranteed).
fn extract_job_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("tool_response")
        .and_then(find_job_id)
        .or_else(|| find_job_id(payload))
}

/// Recursively search a hook-payload JSON for a string `job_id` field. A string
/// that is itself JSON is parsed and descended (the MCP tool result nests the
/// response envelope as a JSON-encoded string), so this stays agnostic to the
/// exact `tool_response` shape, which the host does not pin down.
fn find_job_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get("job_id") {
                return Some(s.clone());
            }
            map.values().find_map(find_job_id)
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_job_id),
        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(s)
            .ok()
            .as_ref()
            .and_then(find_job_id),
        _ => None,
    }
}

/// Inputs for one delegated `delegate`. Grouped into a struct so `run_delegate`
/// avoids a too-many-arguments signature as the surface grew (cwd/env/args/
/// timeouts/isolation). Both deadlines stay raw here: their defaults depend on
/// whether the composed arg list leaves the child streaming.
struct DelegateOpts<'a> {
    profile: &'a str,
    prompt: &'a str,
    model: Option<&'a str>,
    cwd: Option<&'a str>,
    env: HashMap<String, String>,
    extra_args: Vec<String>,
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    resume: Option<&'a str>,
    isolation: Isolation,
    depth: u32,
}

/// Which deadline killed a delegate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expiry {
    /// Nothing arrived on stdout for the idle window.
    Idle,
    /// The run outlived its wall-clock ceiling while still producing output.
    Wall,
}

/// Resolve a delegate's `(wall, idle)` deadlines. Without the event stream there
/// is no liveness signal, so an unset wall clock falls back to the idle default
/// rather than leaving a hung child to sit for the full hour.
fn resolve_deadlines(
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    streaming: bool,
) -> (Duration, Duration) {
    let idle = idle_secs
        .unwrap_or(DEFAULT_IDLE_SECS)
        .clamp(1, MAX_RUN_TIMEOUT_SECS);
    let wall = match timeout_secs {
        Some(secs) => secs.clamp(1, MAX_RUN_TIMEOUT_SECS),
        None if streaming => DEFAULT_RUN_TIMEOUT_SECS,
        None => idle,
    };
    (Duration::from_secs(wall), Duration::from_secs(idle))
}

/// Which deadline (if either) a still-running delegate has tripped.
/// `last_progress` is how far into the run its most recent output arrived; the
/// idle leg is off entirely without the stream, where silence means nothing.
fn expiry(
    elapsed: Duration,
    last_progress: Duration,
    wall: Duration,
    idle: Duration,
    streaming: bool,
) -> Option<Expiry> {
    // Wall clock first: it is the outer bound, and a delegate that stalls near
    // the ceiling trips both in the same poll.
    if elapsed >= wall {
        return Some(Expiry::Wall);
    }
    (streaming && elapsed.saturating_sub(last_progress) >= idle).then_some(Expiry::Idle)
}

/// True when the caller pins its own `--output-format` in `args`. clauth then
/// spawns no format flag of its own, and the child's output shape is unknown, so
/// the idle deadline is off (silence would no longer mean "stuck").
fn sets_output_format(extra_args: &[String]) -> bool {
    extra_args
        .iter()
        .any(|a| a == "--output-format" || a.starts_with("--output-format="))
}

/// What the stdout reader keeps from a streamed delegate. The transcript runs to
/// megabytes and only the terminal envelope is wanted, so lines are inspected and
/// dropped rather than buffered, alongside a bounded tail of the assistant text
/// that lets a killed run still return something.
#[derive(Default)]
struct StreamCapture {
    /// The child's own session id, from the first event carrying one. The handle
    /// a later `resume` needs, and the only way to get it out of a run that never
    /// reached its terminal envelope.
    session_id: Option<String>,
    /// The newest `rate_limit_event` line. Kept because stdout is no longer
    /// buffered whole: without it a throttle that shows up only there would stop
    /// being detectable on a non-zero exit.
    rate_limit_line: Option<String>,
    /// Last line tagged `type:"result"`.
    result_line: Option<String>,
    /// Last parseable non-delta line, whatever its type: the same fallback
    /// [`result_event`] applies to a transcript array.
    last_line: String,
    /// Assistant text from completed message blocks.
    text: String,
    /// Deltas of the block still in flight. Cleared by that block's own
    /// `assistant` event, which carries the same text, so nothing lands twice.
    pending: String,
}

impl StreamCapture {
    /// The whole of stdout as one buffer, for a caller-pinned output format.
    fn from_raw(bytes: &[u8]) -> Self {
        Self {
            last_line: String::from_utf8_lossy(bytes).into_owned(),
            ..Self::default()
        }
    }

    /// The bytes to parse as the delegate's terminal envelope.
    fn envelope_src(&self) -> &str {
        self.result_line.as_deref().unwrap_or(&self.last_line)
    }

    /// Assistant text produced so far: completed blocks plus the in-flight one.
    fn partial_text(&self) -> String {
        format!("{}{}", self.text, self.pending)
    }

    fn push_line(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        // Every event carries it, deltas included, so this is read before the
        // type match returns early for one.
        if self.session_id.is_none()
            && let Some(id) = value.get("session_id").and_then(serde_json::Value::as_str)
        {
            self.session_id = Some(id.to_string());
        }
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("rate_limit_event") => {
                self.rate_limit_line = Some(line.to_string());
                return;
            }
            // Token-level deltas: liveness plus the in-flight block's text. Kept
            // out of `last_line` so a parse-failure report shows a real event.
            Some("stream_event") => {
                self.push_delta(&value);
                return;
            }
            Some("result") => self.result_line = Some(line.to_string()),
            Some("assistant") => self.push_assistant(&value),
            _ => {}
        }
        self.last_line = line.to_string();
    }

    /// Fold a completed assistant message's text blocks into the salvage buffer.
    fn push_assistant(&mut self, value: &serde_json::Value) {
        self.pending.clear();
        let Some(blocks) = value.pointer("/message/content").and_then(|c| c.as_array()) else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(serde_json::Value::as_str)
            {
                self.text.push_str(text);
            }
        }
        keep_tail(&mut self.text, PARTIAL_TEXT_CAP);
    }

    /// Append a `content_block_delta` chunk. Thinking deltas are skipped: the
    /// salvage is the answer, not the reasoning behind it.
    fn push_delta(&mut self, value: &serde_json::Value) {
        if value
            .pointer("/event/delta/type")
            .and_then(serde_json::Value::as_str)
            == Some("text_delta")
            && let Some(text) = value
                .pointer("/event/delta/text")
                .and_then(serde_json::Value::as_str)
        {
            self.pending.push_str(text);
            keep_tail(&mut self.pending, PARTIAL_TEXT_CAP);
        }
    }
}

/// Trim a salvage buffer to its last `cap` bytes, on a char boundary.
fn keep_tail(s: &mut String, cap: usize) {
    if s.len() <= cap {
        return;
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s.replace_range(..start, "");
}

/// Read the child's stdout to EOF, stamping `progress` with the elapsed
/// milliseconds at every line so the wait loop can tell a working delegate from
/// a stalled one. Non-streaming mode drains the pipe whole (there is nothing to
/// stamp until the child exits).
fn read_stdout<R: std::io::Read>(
    reader: R,
    streaming: bool,
    start: Instant,
    progress: &AtomicU64,
) -> StreamCapture {
    let mut reader = reader;
    if !streaming {
        return StreamCapture::from_raw(&drain_pipe(&mut reader));
    }
    let mut buffered = std::io::BufReader::new(reader);
    let mut capture = StreamCapture::default();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        // read_until over lines(): a single event can carry a multi-megabyte tool
        // result, and invalid UTF-8 must not end the capture early.
        match std::io::BufRead::read_until(&mut buffered, b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        progress.store(elapsed_ms(start), Ordering::Relaxed);
        capture.push_line(String::from_utf8_lossy(&raw).trim());
    }
    capture
}

/// Milliseconds since `start`, saturating (a delegate is capped at an hour, so
/// the cast never truncates in practice).
fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Envelope for a delegate clauth killed. `is_error` like any other failure, but
/// it carries the text the run had already produced: the target account's window
/// is spent whether or not clauth keeps the output, so discarding it is a second
/// loss on top of the first.
///
/// `resumable` is whether the killed run's transcript outlives its runtime tree
/// (see [`transcript_survives`]). When it does, the session id is a handle the
/// caller can hand back as `resume`; when it does not, saying so is what stops
/// the operator learning about the toggle by losing a second run.
fn timeout_envelope(
    profile: &str,
    expiry: Expiry,
    elapsed: Duration,
    limit: Duration,
    capture: &StreamCapture,
    resumable: bool,
) -> serde_json::Value {
    let elapsed_secs = elapsed.as_secs();
    let limit_secs = limit.as_secs();
    let (kind, mut reason) = match expiry {
        Expiry::Idle => (
            "idle",
            format!(
                "delegate killed after {elapsed_secs}s: it produced no output for {limit_secs}s. \
                 raise `idle_secs` if the task makes one blocking call longer than that"
            ),
        ),
        Expiry::Wall => (
            "wall_clock",
            format!(
                "delegate killed at its {limit_secs}s wall-clock ceiling. \
                 raise `timeout_secs` for a longer run"
            ),
        ),
    };
    let partial = capture.partial_text();
    if !partial.is_empty() {
        reason.push_str(". the text it had written is in `partial_result`");
    }
    match (&capture.session_id, resumable) {
        (Some(_), true) => {
            reason.push_str(". pick the run back up with `resume: \"<session_id>\"`");
        }
        (_, false) => reason.push_str(
            ". its transcript went with the isolated runtime, so it cannot be resumed; \
             `clauth config` auto-rescue keeps one",
        ),
        (None, true) => {}
    }
    let mut payload = serde_json::json!({
        "profile": profile,
        "is_error": true,
        "timed_out": kind,
        "elapsed_secs": elapsed_secs,
        "result": reason,
    });
    if !partial.is_empty() {
        payload["partial_result"] = serde_json::Value::String(partial);
    }
    if resumable && let Some(id) = &capture.session_id {
        payload["session_id"] = serde_json::Value::String(id.clone());
    }
    payload
}

/// Whether a delegate's transcript outlives its runtime tree, which decides
/// whether the run can ever be resumed. A shared runtime's `projects/` is a
/// symlink into the global store, so its transcript is already there; an
/// isolated one writes to a throwaway tree `ProfileRuntime::drop` removes, and
/// only the opt-in rescue lifts it out first.
fn transcript_survives(isolation: Isolation, auto_rescue: bool) -> bool {
    isolation != Isolation::Isolated || auto_rescue
}

/// Resolve a `resume` id to the workspace its transcript was recorded under.
/// Claude Code resolves `--resume <id>` only within `projects/<slug-of-cwd>/`, so
/// a resume spawned anywhere else is told the conversation does not exist.
///
/// `latest` is refused, though `clauth resume` takes it: the newest session in
/// the whole store is usually the operator's own live one, and spending an
/// account's window continuing that is never what a delegate meant by it.
fn resolve_resume_workspace(session_id: &str) -> std::result::Result<std::path::PathBuf, String> {
    if session_id == "latest" {
        return Err(
            "resume needs an exact session id; `latest` is a `clauth resume` shorthand".to_string(),
        );
    }
    let workspace = crate::sessions::workspace_of(session_id).ok_or_else(|| {
        format!("can't resume '{session_id}': no transcript for it, or none recording a workspace")
    })?;
    if !workspace.is_dir() {
        return Err(format!(
            "can't resume '{session_id}': workspace '{}' no longer exists",
            workspace.display()
        ));
    }
    Ok(workspace)
}

/// Refuse a `cwd` that disagrees with the workspace a `resume` must run in,
/// rather than spawning where Claude Code will not find the transcript. Both
/// sides are canonicalized: one spelling of a path is not the same string as
/// another spelling of it.
fn check_resume_cwd(given: &str, workspace: &std::path::Path) -> std::result::Result<(), String> {
    let given_real = std::fs::canonicalize(given)
        .map_err(|e| format!("cwd '{given}' cannot be resolved: {e}"))?;
    let workspace_real = std::fs::canonicalize(workspace).map_err(|e| {
        format!(
            "workspace '{}' cannot be resolved: {e}",
            workspace.display()
        )
    })?;
    if given_real != workspace_real {
        return Err(format!(
            "cwd '{given}' is not the workspace this session was recorded in ('{}'); \
             drop `cwd` and clauth uses the recorded one",
            workspace.display()
        ));
    }
    Ok(())
}

/// Compose a delegate's environment on `command`: drop inherited provider
/// routing + the active profile's custom env
/// ([`crate::runtime::scrub_profile_env`]), layer the caller's `env`, then
/// clauth's own keys which always win. `CLAUDE_CONFIG_DIR` and the depth guard
/// can't be overridden, and `CLAUDE_CODE_MAX_OUTPUT_TOKENS` only defaults when
/// the caller didn't set it.
fn apply_delegate_env(
    command: &mut Command,
    caller_env: &HashMap<String, String>,
    active_env_keys: &[String],
    config_dir: &std::path::Path,
    depth: u32,
) {
    crate::runtime::scrub_profile_env(command, active_env_keys);
    command.envs(caller_env);
    if !caller_env.contains_key("CLAUDE_CODE_MAX_OUTPUT_TOKENS") {
        command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", DEFAULT_MAX_OUTPUT_TOKENS);
    }
    command
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env(MCP_DEPTH_ENV, (depth + 1).to_string());
}

/// Blocking delegate: acquire the target profile's runtime, spawn a headless
/// `claude -p` with piped stdio, enforce the idle + wall-clock deadlines, and
/// parse its JSON envelope. Returns `Ok(envelope)` on a clean parse or a
/// [`timeout_envelope`] for a killed run, and `Err(reason)` for a non-zero exit
/// or unparseable output (the caller wraps that in an `is_error` envelope).
/// Records observed throughput / rate-limit hits as a side effect, and runs
/// `start::run`'s own transcript-stamp and opt-in isolated-rescue legs on the way
/// out so a delegate's sessions are attributable and (with the toggle on)
/// resumable. Never bubbles a transport-level error.
fn run_delegate(opts: DelegateOpts<'_>) -> std::result::Result<serde_json::Value, String> {
    let config = load_config().map_err(|e| format!("failed to load config: {e}"))?;
    let target = config
        .find(opts.profile)
        .ok_or_else(|| format!("profile not found: {}", opts.profile))?;
    // Mirrors `disable_profile`'s own live-session refusal from the other
    // direction: that guard stops disabling a profile mid-session, this one
    // stops opening a brand-new session on one already disabled.
    if target.is_disabled() {
        return Err(format!("profile is disabled: {}", opts.profile));
    }

    if let Some(dir) = opts.cwd
        && !std::path::Path::new(dir).is_dir()
    {
        return Err(format!("cwd does not exist or is not a directory: {dir}"));
    }

    // A resume must land in the workspace its transcript was recorded under, so
    // that resolution REPLACES the caller's cwd instead of sitting beside it; a
    // `cwd` that disagrees is a mistake worth naming, not one to spawn into.
    let workspace = match opts.resume {
        Some(id) => {
            let workspace = resolve_resume_workspace(id)?;
            if let Some(dir) = opts.cwd {
                check_resume_cwd(dir, &workspace)?;
            }
            Some(workspace)
        }
        None => None,
    };

    // Strip the active profile's custom env so a delegate for `<target>` does
    // not inherit whoever is globally active (mirrors `clauth start`).
    let active_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();

    // Guard kept alive across spawn+wait; dropped on return for RAII teardown.
    // A delegate is a one-shot headless run against a named account, so it never
    // follows the chain — moving it mid-prompt would change who answered.
    let runtime = ProfileRuntime::acquire(target, opts.isolation, &active_env_keys, false)
        .map_err(|e| format!("failed to acquire runtime: {e}"))?;

    let mut command = crate::runtime::claude_command();
    apply_delegate_env(
        &mut command,
        &opts.env,
        &active_env_keys,
        runtime.config_dir(),
        opts.depth,
    );
    // Stream the child's events as NDJSON instead of waiting for one terminal
    // blob: the wait loop needs a liveness signal to tell a working delegate from
    // a hung one, and a killed run must still hand back the text it wrote.
    // `stream-json` refuses to run under `-p` without `--verbose`;
    // `--include-partial-messages` adds the token deltas, so a single long
    // generation counts as progress instead of reading as silence.
    let streaming = !sets_output_format(&opts.extra_args);
    command
        .args(["-p", opts.prompt])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    if streaming {
        command.args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ]);
    }
    // Isolated only: suppress operator/project MCP servers for a clean blind
    // session (mirrors `start.rs`). A shared delegate inherits its config-dir's
    // MCP servers so it can do research/nav. Recursion stays capped either way:
    // the `CLAUTH_MCP_DEPTH` guard refuses a nested `delegate` even when the child
    // loads clauth's own server. Callers can still pass `--mcp-config` (and
    // `--strict-mcp-config`) via `args` to scope a shared delegate.
    if opts.isolation == Isolation::Isolated {
        command.arg("--strict-mcp-config");
    }
    if let Some(m) = opts.model {
        command.args(["--model", m]);
    }
    if let Some(id) = opts.resume {
        command.args(["--resume", id]);
    }
    // Resolve the cwd the spawned `claude` will actually run in: a resume's
    // recorded workspace, else the caller's override, else this process's own cwd
    // (inherited like `start.rs`). If it's the real `$HOME`, guard against the
    // project-settings leak.
    let explicit_cwd = workspace.or_else(|| opts.cwd.map(std::path::PathBuf::from));
    if let Some(dir) = explicit_cwd.as_deref() {
        command.current_dir(dir);
    }
    let effective_cwd = explicit_cwd.or_else(|| std::env::current_dir().ok());
    if let Some(dir) = effective_cwd.as_deref() {
        crate::runtime::guard_home_project_settings(&mut command, dir);
    }
    command.args(&opts.extra_args);

    let run_start = std::time::SystemTime::now();

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

    // Drain both pipes on their own threads from the moment of spawn. A bare
    // try_wait loop never reads, so a >~64KiB result blocks the child on a full
    // pipe and it never exits — a false timeout that drops a valid result. Killing
    // the child closes the write ends, the readers hit EOF, and the joins return.
    let start = Instant::now();
    let progress = std::sync::Arc::new(AtomicU64::new(0));
    let stdout_reader = child.stdout.take().map(|h| {
        let progress = std::sync::Arc::clone(&progress);
        std::thread::spawn(move || read_stdout(h, streaming, start, &progress))
    });
    let stderr_reader = child
        .stderr
        .take()
        .map(|mut h| std::thread::spawn(move || drain_pipe(&mut h)));

    let (wall, idle) = resolve_deadlines(opts.timeout_secs, opts.idle_secs, streaming);

    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                let last_progress = Duration::from_millis(progress.load(Ordering::Relaxed));
                if let Some(expiry) = expiry(start.elapsed(), last_progress, wall, idle, streaming)
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(expiry);
                }
                std::thread::sleep(RUN_POLL_INTERVAL);
            }
            Err(e) => return Err(format!("failed to wait for claude: {e}")),
        }
    };

    // Joined before the timeout branch returns: the kill above closed the write
    // ends, so the readers are at EOF and the capture holds everything the run
    // produced before it died.
    let capture = stdout_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr_bytes = join_reader(stderr_reader);

    // Mirrors `start::run`'s own teardown legs, in the same window: the child has
    // exited and the guard is still alive, so the tree is there to read. Stamp
    // this run's transcripts with the profile that produced them, then (isolated
    // + opt-in) lift the throwaway store into the global one before
    // `drop(runtime)` discards it. Best-effort; a completed delegate never fails
    // on either.
    let isolated = opts.isolation == Isolation::Isolated;
    let projects_dir = if isolated {
        Some(runtime.config_dir().join("projects"))
    } else {
        crate::profile::claude_dir()
            .ok()
            .map(|d| d.join("projects"))
    };
    if let Some(projects_dir) = projects_dir {
        crate::sessions::stamp_run_sessions(opts.profile, &projects_dir, isolated, run_start);
    }
    let auto_rescue = config.state.auto_rescue;
    if isolated
        && auto_rescue
        && let Ok(claude_home) = crate::profile::claude_dir()
    {
        crate::start::rescue_teardown(runtime.config_dir(), runtime.sessions_dir(), &claude_home);
    }

    let status = match outcome {
        Ok(status) => status,
        Err(expiry) => {
            let limit = match expiry {
                Expiry::Idle => idle,
                Expiry::Wall => wall,
            };
            return Ok(timeout_envelope(
                opts.profile,
                expiry,
                start.elapsed(),
                limit,
                &capture,
                transcript_survives(opts.isolation, auto_rescue),
            ));
        }
    };
    let stdout = capture.envelope_src();
    let now = now_epoch_secs();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        // A non-zero exit can be a throttle; record it so `which`/`list_profiles`
        // can flag the model as rate-limited (clauth never sees inference 429s
        // any other way).
        let throttle_scan = format!(
            "{stderr}{stdout}{}",
            capture.rate_limit_line.as_deref().unwrap_or_default()
        );
        if let Some(retry_after) = rate_limit_hint(&throttle_scan) {
            crate::throughput::record_rate_limit(opts.profile, opts.model, retry_after, now);
        }
        return Err(format!(
            "claude exited with {}: {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            truncate(stderr.trim(), 2000)
        ));
    }
    let envelope = parse_delegate_envelope(stdout.trim())?;
    // A clean exit can still carry an in-band error envelope (rate limit shows up
    // there with `--output-format json`); branch on `is_error` so a throttle is
    // recorded as one, not as a (bogus) throughput sample.
    if envelope
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(retry_after) = rate_limit_hint(&envelope.to_string()) {
            crate::throughput::record_rate_limit(opts.profile, opts.model, retry_after, now);
        }
    } else {
        record_throughput_from_envelope(opts.profile, opts.model, &envelope, now);
    }
    Ok(envelope)
}

/// Reduce `claude`'s captured stdout to its single terminal `type:"result"`
/// envelope. Under clauth's own `stream-json` the reader already retained just
/// that line, but a caller-pinned `--output-format json` emits the bare object
/// and a `--verbose` one the full transcript ARRAY (every `system`
/// thinking-token / tool-io / `assistant` event) — valid input that would
/// otherwise be stored and dumped into the caller's context verbatim (a
/// multi-minute run leaks ~1000x the envelope). Collapse all three to the
/// terminal result object so the delegate envelope stays the documented shape
/// regardless of caller `args`.
fn parse_delegate_envelope(stdout: &str) -> std::result::Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(stdout) {
        Ok(serde_json::Value::Array(items)) => result_event(items).ok_or_else(|| {
            format!(
                "no result event in claude output: {}",
                truncate(stdout, 2000)
            )
        }),
        Ok(other) => Ok(other),
        // NDJSON (`stream-json`): not a single JSON value — recover the terminal
        // result event from the per-line events.
        Err(e) => {
            let items = stdout
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .collect();
            result_event(items).ok_or_else(|| {
                format!(
                    "failed to parse claude output: {e}: {}",
                    truncate(stdout, 2000)
                )
            })
        }
    }
}

/// The last `type:"result"` element of a parsed claude event list (its terminal
/// envelope), falling back to the last element when none is tagged. `None` for an
/// empty list.
fn result_event(mut items: Vec<serde_json::Value>) -> Option<serde_json::Value> {
    match items
        .iter()
        .rposition(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("result"))
    {
        Some(i) => Some(items.swap_remove(i)),
        None => items.pop(),
    }
}

/// Pull output-token throughput from a successful `claude` JSON envelope and
/// record it. Best-effort: a missing usage/duration block records nothing.
fn record_throughput_from_envelope(
    profile: &str,
    model: Option<&str>,
    envelope: &serde_json::Value,
    now: i64,
) {
    let output_tokens = envelope
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let duration_ms = envelope
        .get("duration_api_ms")
        .or_else(|| envelope.get("duration_ms"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    crate::throughput::record_success(profile, model, output_tokens, duration_ms, now);
}

/// Detect a rate-limit / 429 signature in a delegate's output. `Some(retry)`
/// when it looks rate-limited (inner `None` = no Retry-After hint found),
/// `None` when it doesn't.
fn rate_limit_hint(text: &str) -> Option<Option<u64>> {
    let lower = text.to_lowercase();
    let limited = lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("overloaded");
    if !limited {
        return None;
    }
    let retry_after = lower.find("retry").and_then(|i| {
        lower[i..]
            .split(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse::<u64>().ok())
    });
    Some(retry_after)
}

/// One-line throughput warning for the live footer, or `None` when nothing is
/// degraded or rate-limited.
fn throughput_note(profile: &str, now: i64) -> Option<String> {
    let flagged: Vec<String> = crate::throughput::summary(profile, now)
        .into_iter()
        .filter(|m| m.degraded || m.rate_limited_recent)
        .map(|m| {
            if m.rate_limited_recent {
                match m.retry_after_s {
                    Some(s) => format!("{} rate-limited (retry ~{s}s)", m.model),
                    None => format!("{} rate-limited", m.model),
                }
            } else {
                format!("{} slow (~{:.0} tok/s)", m.model, m.tok_s)
            }
        })
        .collect();
    (!flagged.is_empty()).then(|| format!("⚠ throughput: {}", flagged.join(", ")))
}

/// Read a child pipe to EOF into a buffer, swallowing read errors (a partial
/// buffer is more useful than a hard failure for an error envelope).
fn drain_pipe<R: std::io::Read>(reader: &mut R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

/// Join a reader thread, returning its drained bytes (empty on a join panic or
/// an absent pipe).
fn join_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// Truncate a string to `max` bytes (on a char boundary) for an error payload,
/// appending an ellipsis when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[tool_handler]
impl ServerHandler for ClauthServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive]; build from default then set fields.
        // Tools capability must be advertised explicitly: ServerInfo::default() leaves
        // capabilities empty, so a spec-compliant client (Claude Code) exposes no tools
        // at all even though the server can answer tools/list.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(build_instructions());
        info
    }
}

/// Build the init-time `instructions` block once from the on-demand config and
/// usage disk cache. Best-effort: a config load failure degrades to a prose-only
/// block rather than failing the handshake.
fn build_instructions() -> String {
    let Ok(config) = load_config() else {
        return "clauth manages multiple Claude Code accounts (\"profiles\"). \
            Call `list_profiles` for live usage figures."
            .to_string();
    };
    let snapshots: Vec<ProfileSnapshot> = config
        .profiles
        .iter()
        .map(|p| {
            let name = p.name.as_str();
            ProfileSnapshot {
                name: name.to_string(),
                active: config.is_active(name),
                provider: provider_label(p),
                base_url: p.base_url.clone(),
                sub_type: tier_label(p),
            }
        })
        .collect();

    render::instructions_block(&snapshots, &crate::which::session_auth())
}

/// Whether this `clauth mcp` process should hold a bare-session marker. Pure, so
/// both refusals are exercised without an env or a spawn.
///
/// `Global` is the whole signal: a server reading the global `~/.claude`
/// credentials is the MCP half of a bare `claude`, while every isolated tier
/// reads its own file — a supervised `clauth start` session, already registered,
/// or a `delegate` child, which gets `CLAUDE_CONFIG_DIR` in the same builder as
/// its depth marker and so needs no depth check of its own here.
fn bare_marker_wanted(auth: &crate::which::SessionAuth, is_probe: bool) -> bool {
    matches!(auth, crate::which::SessionAuth::Global) && !is_probe
}

/// This server's bare-session marker, or `None` when the process is not a bare
/// session or the registration failed. A failure is logged and never fatal: the
/// tally is a display feature riding on the MCP server, and a broken count must
/// not take the server down.
fn hold_bare_session_marker() -> Option<std::fs::File> {
    let is_probe = std::env::var_os(MCP_PROBE_ENV).is_some();
    if !bare_marker_wanted(&crate::which::session_auth(), is_probe) {
        return None;
    }
    match crate::runtime::register_bare_session() {
        Ok(file) => Some(file),
        Err(e) => {
            logline!("clauth: bare-session marker not registered: {e:#}");
            None
        }
    }
}

pub(crate) fn serve() -> Result<()> {
    crate::runtime::gc_stale_runtimes();
    jobs::gc(now_ms());
    // Held across `block_on`, so the flock drops with the process however it dies
    // — a bare `claude` runs no clauth teardown, SIGKILL least of all.
    let _bare_marker = hold_bare_session_marker();
    // rmcp's service loop arms a Tokio timer (needs `enable_time`), so a bare
    // current-thread runtime panics right after the initialize reply. `enable_all`
    // also turns on the I/O driver, covering a future transport that polls a real
    // fd or any added tokio net/process path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_server())
}

async fn run_server() -> Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    let service = ClauthServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_run.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_switch_tool.rs"]
mod switch_tool_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_which_tool.rs"]
mod which_tool_tests;
