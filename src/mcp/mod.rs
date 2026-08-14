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
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CacheScope, CallToolResult, ContentBlock, DiscoverResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::logline::logline;
use crate::out::outln;
use crate::profile::{AppConfig, load_config};
use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache};
use crate::profile_json::{provider_label, tier_label, windows_json};
use crate::providers::ThirdPartyStats;
use crate::runtime::{Isolation, ProfileRuntime};
use crate::usage::{UsageInfo, UsageWindow, now_epoch_secs, now_ms};
use render::{ProfileSnapshot, RosterRank};

/// Marks the `clauth mcp` child that [`crate::plugin_probe::mcp_boots`] spawns
/// for the Plugin tab's handshake check. clauth owns both sides of that spawn, so
/// an env marker beats inferring it from the client identity in a request.
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
/// Cap on one `prompt_file` in bytes. Well under Linux's ~128 KiB single-argument
/// ceiling (the prompt becomes one `-p` argv element), so a file that passes can
/// always be handed to `claude`, and far above any real reusable prompt.
const PROMPT_FILE_CAP: u64 = 64 * 1024;
/// Cap on one `profiles` fan-out. Each target is a real usage window with no
/// undo, so a runaway list is bounded here.
const MAX_FANOUT: usize = 8;

/// Compact per-model throughput rows for a profile (observed tok/s, degraded /
/// rate-limited flags). Empty array when clauth has launched no runs for it.
fn throughput_json(profile: &str, now: i64) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = crate::throughput::summary(profile, now)
        .into_iter()
        .map(throughput_row)
        .collect();
    serde_json::Value::Array(rows)
}

/// One row of [`throughput_json`], shared with [`throughput_warnings`] so the
/// two surfaces cannot drift into describing a model differently.
fn throughput_row(m: crate::throughput::ModelSummary) -> serde_json::Value {
    serde_json::json!({
        "model": m.model,
        "tok_s": (m.tok_s * 10.0).round() / 10.0,
        "samples": m.samples,
        "degraded": m.degraded,
        "rate_limited_recent": m.rate_limited_recent,
        "retry_after_s": m.retry_after_s,
    })
}

/// The subset of [`throughput_json`] a roster is worth spending tokens on: only
/// models a past `delegate` found degraded or recently rate-limited. A healthy
/// row tells a picker nothing it would act on, and one operator's 19 healthy
/// rows measured 31% of the whole `list_profiles` response.
fn throughput_warnings(profile: &str, now: i64) -> Vec<serde_json::Value> {
    crate::throughput::summary(profile, now)
        .into_iter()
        .filter(|m| m.degraded || m.rate_limited_recent)
        .map(throughput_row)
        .collect()
}

/// Fresh-from-cache 5h/7d windows for a profile. Each call re-reads the disk
/// cache (no caching across tool calls per the design).
fn load_windows(name: &str) -> (Option<UsageWindow>, Option<UsageWindow>) {
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(u) => (u.five_hour, u.seven_day),
        None => (None, None),
    }
}

/// The roster's sort key for one profile. A real window first (5h, the pool a
/// `delegate` actually competes for, then 7d), then a third-party provider's own
/// cached bars, then a wallet balance off its cached rows.
fn roster_rank(name: &str) -> RosterRank {
    let (five_h, seven_d) = load_windows(name);
    if let Some(w) = five_h.or(seven_d) {
        return RosterRank::Window(100.0 - w.utilization);
    }
    let Some(stats) = load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE) else {
        return RosterRank::Unknown;
    };
    if let Some(bar) = stats
        .bars
        .iter()
        .find(|b| b.label == "5h")
        .or_else(|| stats.bars.iter().find(|b| b.label == "7d"))
    {
        return RosterRank::Window(100.0 - bar.pct);
    }
    stats
        .rows
        .iter()
        .find(|r| r.label == "total")
        .and_then(|r| parse_balance(&r.value))
        .map_or(RosterRank::Unknown, |(currency, amount)| {
            RosterRank::Balance { currency, amount }
        })
}

/// `"31.45 USD"` → `("USD", 31.45)`: one finite amount plus one 2-5 letter
/// ASCII currency code. The narrowness is the point: a `total` row carrying
/// anything else (z.ai's `123.4M  (1.2k calls)`, a second word, `nan`/`inf`)
/// describes no wallet. A loose parse would invent one to rank on. Taking the
/// FIRST such row is also what lands a profile holding two wallets in exactly
/// one currency group.
fn parse_balance(value: &str) -> Option<(String, f64)> {
    let mut parts = value.split_whitespace();
    let amount: f64 = parts.next()?.parse().ok()?;
    if !amount.is_finite() {
        return None;
    }
    let currency = parts.next()?;
    if parts.next().is_some()
        || !(2..=5).contains(&currency.len())
        || !currency.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    Some((currency.to_string(), amount))
}

/// Output format for every tool. `prose` is the default; `json` is the opt-in a
/// caller must name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Prose,
    Json,
}

impl Format {
    /// Resolve the tool's `format` argument. Unset means prose. An unrecognised
    /// value is refused by name so a typo cannot silently degrade to prose.
    fn parse(raw: Option<&str>) -> std::result::Result<Self, String> {
        match raw {
            None | Some("prose") => Ok(Format::Prose),
            Some("json") => Ok(Format::Json),
            Some(other) => Err(format!(
                "unrecognized format \"{other}\": accepted \"prose\" and \"json\""
            )),
        }
    }
}

/// Refuse an unrecognised `format` value. There is no format to honour yet, so
/// the refusal is a JSON text block like every other error envelope.
fn format_refusal(reason: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(
        serde_json::json!({ "ok": false, "reason": reason }).to_string(),
    )])
}

/// A `delegate` argument/validation refusal: one `{is_error, result}` envelope in
/// one content block, honouring the caller's `format`. Prose reads as a sentence;
/// JSON keeps the same keys as every other delegate refusal.
fn delegate_refusal(format: Format, reason: &str) -> CallToolResult {
    let payload = serde_json::json!({ "is_error": true, "result": reason });
    let prose = render::delegate_refusal_prose(&payload);
    CallToolResult::error(single_block(payload, format, prose))
}

/// The live-usage footer folded into a payload as data: which profile the
/// percentages describe, and the 5h/7d share used (null when uncached). The
/// throughput warning is added by the caller when the tool has one.
fn live_usage_json(
    profile: Option<&str>,
    five_h: Option<&UsageWindow>,
    seven_d: Option<&UsageWindow>,
) -> serde_json::Value {
    serde_json::json!({
        "profile": profile,
        "5h_used_pct": five_h.map(|w| w.utilization),
        "7d_used_pct": seven_d.map(|w| w.utilization),
    })
}

/// Collapse one payload to exactly one content block in the requested format.
/// `prose` is the payload's prose spelling (already computed).
fn single_block(payload: serde_json::Value, format: Format, prose: String) -> Vec<ContentBlock> {
    vec![ContentBlock::text(match format {
        Format::Json => payload.to_string(),
        Format::Prose => prose,
    })]
}

/// Fold the active profile's live usage into a payload, replacing the old
/// second-block footer.
fn fold_active_live_usage(mut payload: serde_json::Value, config: &AppConfig) -> serde_json::Value {
    let active = config.state.active_profile.as_deref();
    let (five_h, seven_d) = match active {
        Some(name) => load_windows(name),
        None => (None, None),
    };
    payload["live_usage"] = live_usage_json(active, five_h.as_ref(), seven_d.as_ref());
    payload
}

/// Fold the target profile's live usage into a delegate envelope (the sync
/// `delegate` and `delegate_result` done-handoff paths share this). The
/// envelope is whatever `claude` printed, so it may be ANY json shape:
/// `parse_delegate_envelope` returns non-objects verbatim. A non-object is
/// wrapped under `result` (the documented self-report key) first — `serde_json`'s
/// string-key `IndexMut` auto-vivifies only `Null` and panics on every other
/// non-object, and the delegate's own output must survive the fold either way.
fn fold_delegate_live_usage(
    payload: serde_json::Value,
    profile: &str,
    now: i64,
) -> serde_json::Value {
    let mut map = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_string(), other);
            map
        }
    };
    let (five_h, seven_d) = load_windows(profile);
    let mut live = live_usage_json(Some(profile), five_h.as_ref(), seven_d.as_ref());
    if let Some(note) = throughput_note(profile, now) {
        live["throughput_warning"] = serde_json::Value::String(note);
    }
    map.insert("live_usage".to_string(), live);
    serde_json::Value::Object(map)
}

#[derive(Clone)]
pub(crate) struct ClauthServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SwitchArgs {
    /// Profile name to relink the global active credentials to.
    name: String,
    /// Output format: `prose` (default) or `json`.
    format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ListProfilesArgs {
    /// Restrict the roster to these profiles (case-insensitive). Omit it, or
    /// pass an empty list, for every profile.
    names: Option<Vec<String>>,
    /// Output format: `prose` (default) or `json`.
    format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateArgs {
    /// Profile name to run the headless delegate session under. Exactly one of
    /// `profile` or `profiles`.
    profile: Option<String>,
    /// Fan out one delegate per named account, background-only. Exactly one of
    /// `profile` or `profiles`; a fan-out spends one usage window per account.
    profiles: Option<Vec<String>>,
    /// Prompt passed to the delegated `claude -p` session. Exactly one of
    /// `prompt` or `prompt_file`.
    prompt: Option<String>,
    /// Path (relative to `cwd`) of a file whose contents are the prompt. Read
    /// once and reused across a fan-out. Exactly one of `prompt` or
    /// `prompt_file`.
    prompt_file: Option<String>,
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
    /// what normally ends a stuck delegate. Defaults to 3600 while the event
    /// stream is on; with a caller-pinned `--output-format` there is no liveness
    /// signal, so an unset wall clock falls back to `idle_secs` instead.
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
    /// Output format: `prose` (default) or `json`.
    format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateResultArgs {
    /// Job id returned by a `delegate` call made with `background: true`.
    /// Exactly one of `job_id` or `job_ids`.
    job_id: Option<String>,
    /// Collect several backgrounded jobs in one call: one result per id, in the
    /// order given (the done envelope, a running status, or `unknown` for an
    /// absent id). Capped at 256 ids: the job store keeps at most that many
    /// files, so a longer list buys nothing. Exactly one of `job_id` or
    /// `job_ids`.
    job_ids: Option<Vec<String>>,
    /// Seconds to long-poll for completion before returning (0..=60, default 0 =
    /// reply instantly with the current state).
    wait_secs: Option<u64>,
    /// Output format: `prose` (default) or `json`.
    format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhichArgs {
    /// Output format: `prose` (default) or `json`.
    format: Option<String>,
}

#[tool_router]
impl ClauthServer {
    pub(crate) fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Every clauth profile from disk cache; zero quota, no network. Call it at \
session start, and pass `names` to re-check one profile instead of pulling the whole roster. \
Reading the JSON: `utilization_pct` in `windows[]` is the percent of that window already USED, so \
higher means less headroom. `tier` is the plan label; a canceled subscription reports the org's \
post-cancellation tier (`Free`), never the word `canceled`. `host` is the endpoint's host, absent \
for a default OAuth profile. `third_party` is a cached balance or quota headline for provider-key \
profiles. Two fields appear only when they carry news: `has_live_session` when a clauth-managed \
session already owns the profile, and `throughput[]` (observed tok/s from past `delegate` calls) \
only for a model that is `degraded` or `rate_limited_recent` — either makes it a bad pick. \
Replies in prose by default; pass `format: \"json\"` for the structured roster."
    )]
    async fn list_profiles(
        &self,
        Parameters(ListProfilesArgs { names, format }): Parameters<ListProfilesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let format = match Format::parse(format.as_deref()) {
            Ok(f) => f,
            Err(reason) => return Ok(format_refusal(reason)),
        };
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let now = now_epoch_secs();

        // Resolve the filter before rendering anything. A name matching nothing
        // is a caller mistake, and silently dropping it would answer with a
        // roster that reads exactly like "that profile is gone".
        let wanted = match names.as_deref() {
            None | Some([]) => None,
            Some(raw) => {
                let (found, unknown): (Vec<_>, Vec<_>) = raw
                    .iter()
                    .map(|n| config.canonical_name(n).ok_or_else(|| n.clone()))
                    .partition(Result::is_ok);
                if !unknown.is_empty() {
                    let missing: Vec<String> =
                        unknown.into_iter().map(Result::unwrap_err).collect();
                    let payload = serde_json::json!({
                        "ok": false,
                        "reason": format!(
                            "profile not found: {}; omit `names` for the full roster",
                            missing.join(", ")
                        ),
                    });
                    let prose = render::list_profiles_prose(&payload);
                    return Ok(CallToolResult::error(single_block(payload, format, prose)));
                }
                Some(
                    found
                        .into_iter()
                        .map(Result::unwrap)
                        .collect::<Vec<String>>(),
                )
            }
        };

        let profiles: Vec<serde_json::Value> = config
            .profiles
            .iter()
            .filter(|p| {
                wanted
                    .as_ref()
                    .is_none_or(|w| w.iter().any(|n| n == p.name.as_str()))
            })
            .map(|p| {
                let name = p.name.as_str();
                let third_party = if p.is_third_party() {
                    load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                        .as_ref()
                        .map(render::third_party_headline)
                } else {
                    None
                };
                let mut row = serde_json::json!({
                    "name": name,
                    "active": config.is_active(name),
                    "provider": provider_label(p),
                    "tier": tier_label(p),
                    "windows": windows_json(name),
                    "third_party": third_party,
                });
                // Host, not the full endpoint: every profile of one provider
                // repeats the same path, and the cost model only ever asks
                // whether the host is loopback or LAN.
                if let Some(url) = &p.base_url {
                    row["host"] = serde_json::json!(render::base_url_host(url));
                }
                // Both of these are absent unless they say something. Emitted
                // unconditionally they were 39% of a 27-profile response, nearly
                // all of it `false` and rows carrying no warning.
                if crate::runtime::has_live_session(name) {
                    row["has_live_session"] = serde_json::json!(true);
                }
                let warnings = throughput_warnings(name, now);
                if !warnings.is_empty() {
                    row["throughput"] = serde_json::Value::Array(warnings);
                }
                row
            })
            .collect();

        let payload = serde_json::json!({ "profiles": profiles });
        let prose = render::list_profiles_prose(&payload);
        Ok(CallToolResult::success(single_block(
            payload, format, prose,
        )))
    }

    #[tool(
        description = "Which profile owns the credentials THIS session loaded, which is not \
always the active one. `source` says how it resolved: `refresh_match` / `session_token_match` (a \
profile's stored credential matches the live one), `session_dir` (this session's runtime dir pins \
the profile), `credential_less_active` (the configured active profile, nothing on disk to match). \
The reply carries the active profile's live 5h/7d usage. Prose by default; pass `format: \
\"json\"` for the structured payload."
    )]
    async fn which(
        &self,
        Parameters(WhichArgs { format }): Parameters<WhichArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let format = match Format::parse(format.as_deref()) {
            Ok(f) => f,
            Err(reason) => return Ok(format_refusal(reason)),
        };
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
        let payload = fold_active_live_usage(
            serde_json::json!({
                "profile": resolved.as_ref().map(|(name, _)| name),
                "source": resolved.as_ref().map(|(_, source)| source.as_str()),
                "tier": tier,
                "throughput": throughput,
            }),
            &config,
        );
        let prose = render::which_prose(&payload);
        Ok(CallToolResult::success(single_block(
            payload, format, prose,
        )))
    }

    #[tool(
        description = "Relink the global `~/.claude` credentials to another profile. What that \
does to THIS session depends on how it reads credentials; the server instructions say which case \
this session is in. Prose by default; pass `format: \"json\"` for the structured payload."
    )]
    async fn switch(
        &self,
        Parameters(SwitchArgs { name, format }): Parameters<SwitchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let format = match Format::parse(format.as_deref()) {
            Ok(f) => f,
            Err(reason) => return Ok(format_refusal(reason)),
        };
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Resolve the raw tool argument to a stored profile (case-insensitive)
        // BEFORE any mutation — the same guard the CLI applies. Skipping it lets an
        // unknown/wrong-case name reach `link_profile_credentials`, which strips the
        // live `.credentials.json` symlink and creates no replacement (it only errors
        // later at `finish_switch`), leaving the global session credential-less.
        let Some(name) = config.canonical_name(&name) else {
            let payload =
                serde_json::json!({ "ok": false, "reason": format!("profile not found: {name}") });
            let payload = fold_active_live_usage(payload, &config);
            let prose = render::switch_prose(&payload);
            return Ok(CallToolResult::error(single_block(payload, format, prose)));
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
                let payload = fold_active_live_usage(
                    serde_json::json!({
                        "ok": true,
                        "previous": previous,
                        "active": active,
                    }),
                    &config,
                );
                let prose = render::switch_prose(&payload);
                Ok(CallToolResult::success(single_block(
                    payload, format, prose,
                )))
            }
            Err(e) => {
                let payload = fold_active_live_usage(
                    serde_json::json!({ "ok": false, "reason": e.to_string() }),
                    &config,
                );
                let prose = render::switch_prose(&payload);
                Ok(CallToolResult::error(single_block(payload, format, prose)))
            }
        }
    }

    #[tool(
        description = "Run a task on another clauth profile: a fresh headless `claude` session \
under that account's credentials. It SPENDS that account's window or money, so pick the target \
from `list_profiles`. It sees only the `prompt` you pass and has no view of this conversation, so \
state the whole task there.\n\n\
Give exactly one prompt source: `prompt` inline, or `prompt_file` — a path relative to `cwd` \
whose contents are the prompt. `prompt_file` is read once (validated against `cwd`, size-capped) \
so a long reusable prompt costs your context nothing to pass, and a `profiles` fan-out reuses \
that one read across every account. Exactly one target too: `profile` for one account, or \
`profiles` for a background-only fan-out that spawns one delegate per named account and spends \
one usage window per account, returning one `job_id` per account.\n\n\
Blocking by default. Pass `background: true` for a `{job_id}` now; the result auto-arrives via \
clauth's PostToolUse hook, and `delegate_result({job_id})` is the fallback when hooks are off. \
A fan-out's every envelope auto-arrives that way too; with hooks off, poll each id with \
`delegate_result`. Prefer `background` for a slow or third-party endpoint, where a blocking call \
ties up this turn. \
Add `monitor: true` so a `delegate_result` poll reports `elapsed_secs` + the target's live \
`quota`.\n\n\
`isolated: true` for a one-shot: a clean session with no operator `CLAUDE.md`, plugins, hooks, \
skills or MCP servers, so it is cheaper and bills fewer tokens. Leave it false only when the task \
needs this repo's tools, and scope those with \
`args:[\"--mcp-config\",\"<json|path>\",\"--strict-mcp-config\"]`. Either way the delegate loads \
the project `CLAUDE.md` of its `cwd` (defaults to this server's cwd), so point `cwd` at a clean \
dir for an unrelated one-shot.\n\n\
Depth-capped at 1: a delegate cannot call `delegate` again. Its own subagents do run, under the \
SAME profile.\n\n\
Killed after `idle_secs` of total silence or the `timeout_secs` wall clock; a run that keeps \
streaming is never cut off. A kill returns `timed_out`, whatever text it had in `partial_result` \
(the window is spent either way), and a `session_id` when the run is resumable — pass it as \
`resume` with a new `prompt` instead of paying for the work twice. An `isolated` run is resumable \
only with clauth's auto-rescue on, and the killed envelope says which case it is.\n\n\
Returns the envelope (`result`, `is_error`, `total_cost_usd`, token usage). `result` is the \
delegate's own self-report, so spot-verify it like any subagent.\n\n\
Prose by default; pass `format: \"json\"` for the structured envelope."
    )]
    async fn delegate(
        &self,
        Parameters(DelegateArgs {
            profile,
            profiles,
            prompt,
            prompt_file,
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
            format,
        }): Parameters<DelegateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let format = match Format::parse(format.as_deref()) {
            Ok(f) => f,
            Err(reason) => return Ok(format_refusal(reason)),
        };
        // Fail closed: a present-but-unparseable value is treated as max depth
        // (refuse), so a corrupt env can never re-enable delegation. Only a truly
        // absent var is depth 0.
        let depth: u32 = match std::env::var(MCP_DEPTH_ENV) {
            Ok(v) => v.trim().parse().unwrap_or(u32::MAX),
            Err(_) => 0,
        };
        if depth >= 1 {
            // The refusal fires before target validation, but the caller's own
            // spelling is known here: name the target it asked for instead of a
            // `null` profile. `profile` / `profiles` are optional keys, present
            // only when the caller named that spelling.
            let payload = match (&profile, &profiles) {
                (Some(t), _) => serde_json::json!({
                    "profile": t,
                    "is_error": true,
                    "result": "delegation depth exceeded (max 1)",
                }),
                (None, Some(names)) => serde_json::json!({
                    "profiles": names,
                    "is_error": true,
                    "result": "delegation depth exceeded (max 1)",
                }),
                (None, None) => serde_json::json!({
                    "is_error": true,
                    "result": "delegation depth exceeded (max 1)",
                }),
            };
            let prose = render::delegate_refusal_prose(&payload);
            return Ok(CallToolResult::error(single_block(payload, format, prose)));
        }

        // Exactly one prompt source. A prompt read from a file still costs the
        // target account once, but no longer costs the CALLING model its own
        // context to pass the same long prompt inline.
        if prompt.is_some() == prompt_file.is_some() {
            let reason = if prompt.is_some() {
                "exactly one of `prompt` or `prompt_file` must be given; both were"
            } else {
                "exactly one of `prompt` or `prompt_file` must be given; neither was"
            };
            return Ok(delegate_refusal(format, reason));
        }

        // Exactly one target: a single account, or a `profiles` fan-out.
        if profile.is_some() == profiles.is_some() {
            let reason = if profile.is_some() {
                "exactly one of `profile` or `profiles` must be given; both were"
            } else {
                "exactly one of `profile` or `profiles` must be given; neither was"
            };
            return Ok(delegate_refusal(format, reason));
        }

        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Which accounts to spend, in canonical spelling, resolved BEFORE any
        // spawn or read. A fan-out is background-only: blocking N accounts has no
        // sensible timeout story, so it is refused by name here.
        enum Target {
            One(String),
            Many(Vec<String>),
        }
        let target = if let Some(raw) = profiles.as_deref() {
            if !background.unwrap_or(false) {
                return Ok(delegate_refusal(
                    format,
                    "`profiles` requires `background: true`",
                ));
            }
            match resolve_fanout(&config, raw) {
                Ok(names) => Target::Many(names),
                Err(reason) => return Ok(delegate_refusal(format, &reason)),
            }
        } else {
            // `profile` is Some here: the exactly-one guard above just proved it.
            let raw = profile.as_deref().unwrap_or_default();
            let Some(name) = config.canonical_name(raw) else {
                return Ok(delegate_refusal(
                    format,
                    &format!("profile not found: {raw}"),
                ));
            };
            Target::One(name)
        };

        // Resolve the prompt text once, before any spawn, so a fan-out reuses one
        // read across every account.
        let prompt: std::sync::Arc<str> = match prompt_file.as_deref() {
            Some(rel) => match read_prompt_file(cwd.as_deref(), rel) {
                Ok(text) => text.into(),
                Err(reason) => return Ok(delegate_refusal(format, &reason)),
            },
            None => prompt.as_deref().unwrap_or_default().to_string().into(),
        };

        // Both deadlines resolve inside `run_delegate`: the wall clock's fallback
        // depends on whether the child ends up streaming, which only the composed
        // arg list knows.
        let isolation = if isolated.unwrap_or(false) {
            Isolation::Isolated
        } else {
            Isolation::Shared
        };

        if background.unwrap_or(false) {
            match target {
                Target::One(name) => {
                    let opts = BackgroundOpts {
                        prompt,
                        model,
                        cwd,
                        env: env.unwrap_or_default(),
                        extra_args: args.unwrap_or_default(),
                        timeout_secs,
                        idle_secs,
                        resume,
                        isolation,
                        depth,
                    };
                    let (job_id, started_at) =
                        reserve_background_job(&name, monitor.unwrap_or(false))
                            .map_err(|e| ErrorData::internal_error(e, None))?;
                    launch_background_delegate(name.clone(), opts, job_id.clone(), started_at);
                    let payload = serde_json::json!({
                        "job_id": job_id,
                        "profile": name,
                        "started_at": started_at,
                        "status": "running",
                    });
                    let prose = render::delegate_prose(&payload);
                    return Ok(CallToolResult::success(single_block(
                        payload, format, prose,
                    )));
                }
                Target::Many(names) => {
                    let opts = BackgroundOpts {
                        prompt,
                        model,
                        cwd,
                        env: env.unwrap_or_default(),
                        extra_args: args.unwrap_or_default(),
                        timeout_secs,
                        idle_secs,
                        resume,
                        isolation,
                        depth,
                    };
                    let monitor = monitor.unwrap_or(false);
                    // Reserve every job file BEFORE the first spawn: the reserve
                    // is the only fallible step (ENOSPC / perms on the jobs dir),
                    // so a failure here spends no window and loses no job id. The
                    // ids already reserved exist nowhere else; drop them and keep
                    // the all-or-nothing contract.
                    let mut handles = Vec::with_capacity(names.len());
                    for name in &names {
                        match reserve_background_job(name, monitor) {
                            Ok(handle) => handles.push(handle),
                            Err(reason) => {
                                for (job_id, _) in &handles {
                                    jobs::remove(job_id);
                                }
                                return Ok(delegate_refusal(format, &reason));
                            }
                        }
                    }
                    let mut jobs = Vec::with_capacity(names.len());
                    for (name, (job_id, started_at)) in names.iter().zip(handles) {
                        launch_background_delegate(
                            name.clone(),
                            opts.clone(),
                            job_id.clone(),
                            started_at,
                        );
                        jobs.push(serde_json::json!({
                            "job_id": job_id,
                            "profile": name,
                            "started_at": started_at,
                            "status": "running",
                        }));
                    }
                    let payload = serde_json::json!({ "jobs": jobs });
                    let prose = render::delegate_fanout_prose(&payload);
                    return Ok(CallToolResult::success(single_block(
                        payload, format, prose,
                    )));
                }
            }
        }

        // Blocking single delegate. A fan-out never reaches here: it is refused
        // above unless `background` is set.
        let Target::One(target) = target else {
            return Err(ErrorData::internal_error(
                "fan-out reached the blocking path".to_string(),
                None,
            ));
        };
        let target_for_task = target.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            run_delegate(DelegateOpts {
                profile: &target_for_task,
                prompt: prompt.as_ref(),
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
                "profile": target,
                "is_error": true,
                "result": reason,
            }),
        };

        let payload = fold_delegate_live_usage(envelope, &target, now_epoch_secs());
        let is_error = payload
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let prose = render::delegate_prose(&payload);
        if is_error {
            Ok(CallToolResult::error(single_block(payload, format, prose)))
        } else {
            Ok(CallToolResult::success(single_block(
                payload, format, prose,
            )))
        }
    }

    #[tool(
        description = "Collect a backgrounded `delegate` by `job_id`. Normally unnecessary: \
clauth's PostToolUse hook delivers the result on its own. Use it when hooks are off, or to check \
progress; `wait_secs` (0..=60) long-polls. Returns the delegate envelope when done, else \
`{status:\"running\", elapsed_secs, quota?}` (`quota` only when that `delegate` call set \
`monitor: true`), or an error for an unknown `job_id`. A `job_ids` list collects several jobs in \
one call instead: one result per id, in the order given (the done envelope, a running status, or \
`unknown` for an absent id), capped at 256. Exactly one of `job_id` or `job_ids`. Prose by \
default (a batch reads as one line per job); pass `format: \"json\"` for the structured payload."
    )]
    async fn delegate_result(
        &self,
        Parameters(DelegateResultArgs {
            job_id,
            job_ids,
            wait_secs,
            format,
        }): Parameters<DelegateResultArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let format = match Format::parse(format.as_deref()) {
            Ok(f) => f,
            Err(reason) => return Ok(format_refusal(reason)),
        };
        // Exactly one id source, mirrored from `delegate`'s target pair: a
        // call that named both, or neither, is a caller mistake refused by
        // name before any job store touch.
        if job_id.is_some() == job_ids.is_some() {
            let reason = if job_id.is_some() {
                "exactly one of `job_id` or `job_ids` must be given; both were"
            } else {
                "exactly one of `job_id` or `job_ids` must be given; neither was"
            };
            let payload = serde_json::json!({ "is_error": true, "result": reason });
            let prose = render::delegate_result_prose(&payload);
            return Ok(CallToolResult::error(single_block(payload, format, prose)));
        }
        let wait = wait_secs.unwrap_or(0).min(MAX_RESULT_WAIT_SECS);
        if let Some(jid) = job_id {
            delegate_result_one(jid, wait, format).await
        } else {
            delegate_result_batch(job_ids.unwrap_or_default(), wait, format).await
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

/// The single-`job_id` half of `delegate_result`, byte-compatible with the
/// pre-batch tool: one envelope/status/error in one block, an unknown or unsafe
/// id refused by name.
async fn delegate_result_one(
    job_id: String,
    wait: u64,
    format: Format,
) -> Result<CallToolResult, ErrorData> {
    if !jobs::is_safe_job_id(&job_id) {
        let payload = serde_json::json!({ "is_error": true, "result": "invalid job_id" });
        let prose = render::delegate_result_prose(&payload);
        return Ok(CallToolResult::error(single_block(payload, format, prose)));
    }
    let jid = job_id.clone();
    let outcome = tokio::task::spawn_blocking(move || wait_for_done(&jid, wait))
        .await
        .map_err(|e| ErrorData::internal_error(format!("wait task panicked: {e}"), None))?;

    match outcome {
        WaitOutcome::Unknown => {
            let payload = serde_json::json!({ "is_error": true, "result": format!("unknown job_id: {job_id}") });
            let prose = render::delegate_result_prose(&payload);
            Ok(CallToolResult::error(single_block(payload, format, prose)))
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
            let prose = render::delegate_result_prose(&payload);
            Ok(CallToolResult::success(single_block(
                payload, format, prose,
            )))
        }
        WaitOutcome::Done(record) => {
            let (blocks, is_error) = render_done_envelope(record, format);
            // Fallback path delivered it — evict only now that the envelope
            // is safely rendered, so the file doesn't linger past its
            // purpose (GC also reaps it on a TTL) while a panic inside
            // `render_done_envelope` still leaves the job file as the
            // recoverable copy.
            jobs::remove(&job_id);
            if is_error {
                Ok(CallToolResult::error(blocks))
            } else {
                Ok(CallToolResult::success(blocks))
            }
        }
    }
}

/// The `job_ids` half of `delegate_result`: one result per requested id in the
/// order given. An absent id is its own `unknown` result, never a batch-level
/// failure; a done id is evicted only after the whole batch rendered, so a
/// mid-fold panic leaves every done file as its recoverable copy.
async fn delegate_result_batch(
    job_ids: Vec<String>,
    wait: u64,
    format: Format,
) -> Result<CallToolResult, ErrorData> {
    // The cap mirrors the job store's own retention: GC keeps at most
    // `MAX_RETAINED` files, so a longer list could not resolve more ids, and
    // the bound keeps one response from growing without limit.
    if job_ids.len() > jobs::MAX_RETAINED {
        let reason = format!(
            "`job_ids` capped at {} ids; got {}",
            jobs::MAX_RETAINED,
            job_ids.len()
        );
        let payload = serde_json::json!({ "is_error": true, "result": reason });
        let prose = render::delegate_result_prose(&payload);
        return Ok(CallToolResult::error(single_block(payload, format, prose)));
    }
    // An empty list passes every per-id check vacuously and would return a
    // success-shaped `{"results": []}` that collected nothing.
    if job_ids.is_empty() {
        let reason = "`job_ids` is empty: name at least one job_id";
        let payload = serde_json::json!({ "is_error": true, "result": reason });
        let prose = render::delegate_result_prose(&payload);
        return Ok(CallToolResult::error(single_block(payload, format, prose)));
    }

    let outcomes = tokio::task::spawn_blocking(move || wait_for_batch(&job_ids, wait))
        .await
        .map_err(|e| ErrorData::internal_error(format!("wait task panicked: {e}"), None))?;

    let mut results = Vec::with_capacity(outcomes.len());
    let mut delivered = Vec::new();
    for (id, outcome) in outcomes {
        let entry = match outcome {
            WaitOutcome::Unknown => serde_json::json!({ "job_id": id, "status": "unknown" }),
            WaitOutcome::Running(record) => {
                let elapsed_secs = now_ms().saturating_sub(record.started_at) / 1000;
                let mut payload = serde_json::json!({
                    "job_id": id,
                    "status": "running",
                    "elapsed_secs": elapsed_secs,
                });
                if record.monitor {
                    payload["quota"] = windows_json(&record.profile);
                }
                payload
            }
            WaitOutcome::Done(record) => {
                let (mut payload, _) = fold_done_envelope(&record);
                // The folded envelope is always an object (a non-object
                // self-report is wrapped under `result` first), so the caller's
                // per-id markers cannot collide with delegate output.
                if let serde_json::Value::Object(map) = &mut payload {
                    map.insert("job_id".to_string(), serde_json::Value::String(id));
                    map.insert(
                        "status".to_string(),
                        serde_json::Value::String("done".to_string()),
                    );
                }
                delivered.push(record.job_id);
                payload
            }
        };
        results.push(entry);
    }
    let payload = serde_json::json!({ "results": results });
    let prose = render::delegate_result_batch_prose(&payload);
    let blocks = single_block(payload, format, prose);
    for id in delivered {
        jobs::remove(&id);
    }
    Ok(CallToolResult::success(blocks))
}

/// Fold a finished job's envelope the way every delivery path does, returning
/// the payload and its error flag. Pure of the job store: the caller evicts the
/// file only after its render, so a panic inside leaves the job file as the
/// recoverable copy of the delegate's result.
fn fold_done_envelope(record: &jobs::JobRecord) -> (serde_json::Value, bool) {
    let payload = fold_delegate_live_usage(
        record.envelope.clone().unwrap_or_else(|| {
            serde_json::json!({
                "profile": record.profile,
                "is_error": true,
                "result": "job finished without an envelope",
            })
        }),
        &record.profile,
        now_epoch_secs(),
    );
    let is_error = payload
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (payload, is_error)
}

/// Render a finished job's envelope into its response blocks and error flag.
fn render_done_envelope(record: jobs::JobRecord, format: Format) -> (Vec<ContentBlock>, bool) {
    let (payload, is_error) = fold_done_envelope(&record);
    let prose = render::delegate_result_prose(&payload);
    (single_block(payload, format, prose), is_error)
}

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

/// Poll every id until each resolves or `deadline_secs` elapses, mirroring
/// `await_job_outcomes`'s semantics: a done file resolves at once, an absent
/// file resolves at once (it never appears for a caller-supplied id), and a
/// running file holds until the deadline. One outcome per id, in the order
/// given. Blocking; callers wrap it in `spawn_blocking`.
fn wait_for_batch(job_ids: &[String], deadline_secs: u64) -> Vec<(String, WaitOutcome)> {
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    // `None` = unresolved. An unsafe id can never name a job file
    // (`new_job_id` mints only safe ids), so it resolves to `Unknown` upfront
    // and never reaches the path join.
    let mut outcomes: Vec<Option<WaitOutcome>> = job_ids
        .iter()
        .map(|id| (!jobs::is_safe_job_id(id)).then_some(WaitOutcome::Unknown))
        .collect();
    loop {
        let mut unresolved = false;
        for (id, slot) in job_ids.iter().zip(&mut outcomes) {
            if slot.is_some() {
                continue;
            }
            match jobs::read(id) {
                Some(r) if r.state == jobs::JobState::Done => *slot = Some(WaitOutcome::Done(r)),
                Some(r) if start.elapsed() >= deadline => *slot = Some(WaitOutcome::Running(r)),
                Some(_) => unresolved = true,
                None => *slot = Some(WaitOutcome::Unknown),
            }
        }
        if !unresolved || start.elapsed() >= deadline {
            break;
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
    job_ids
        .iter()
        .zip(outcomes)
        .map(|(id, slot)| (id.clone(), slot.unwrap_or(WaitOutcome::Unknown)))
        .collect()
}

/// `clauth mcp-await-job` — the body of the bundled PostToolUse `asyncRewake`
/// hook. Reads the hook payload on stdin, finds every background `job_id` in it,
/// waits for each, prints the delivered envelopes to stdout, and exits 2 to wake
/// the model. A sync `delegate` (no `job_id` in the payload) is a no-op (exit 0).
/// On its own deadline it exits 2 with a nudge to call `delegate_result`
/// instead.
pub(crate) fn await_job() -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let job_ids = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .as_ref()
        .map(extract_job_ids)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| jobs::is_safe_job_id(id))
        .collect::<Vec<_>>();
    if job_ids.is_empty() {
        std::process::exit(0); // sync delegate or unparseable input: nothing to deliver
    }

    let (delivered, pending) =
        await_job_outcomes(&job_ids, Duration::from_secs(AWAIT_JOB_DEADLINE_SECS));
    for envelope in &delivered {
        outln!("{envelope}");
    }
    if delivered.is_empty() {
        std::process::exit(0); // every id already gone: nothing was delivered
    }
    if pending.is_empty() {
        std::process::exit(2); // wake the model with the result(s)
    }
    let noun = if pending.len() == 1 { "job" } else { "jobs" };
    outln!(
        "delegate {noun} `{}` still running; call `delegate_result` to retrieve {}",
        pending.join("`, `"),
        if pending.len() == 1 { "it" } else { "them" }
    );
    std::process::exit(2);
}

/// Poll every id in `job_ids` until each is `done` or gone, or `deadline`
/// passes. Returns the delivered envelopes and the ids still `running` at the
/// deadline. An absent id is dropped silently (its file was GC'd or already
/// collected). Blocking; the hook calls it directly on its own thread.
fn await_job_outcomes(
    job_ids: &[String],
    deadline: Duration,
) -> (Vec<serde_json::Value>, Vec<String>) {
    let start = Instant::now();
    let mut delivered = Vec::new();
    let mut pending: Vec<&String> = job_ids.iter().collect();
    loop {
        pending.retain(|id| match jobs::read(id) {
            Some(r) if r.state == jobs::JobState::Done => {
                let envelope = r.envelope.unwrap_or_else(|| {
                    serde_json::json!({
                        "profile": r.profile,
                        "is_error": true,
                        "result": "job finished without an envelope",
                    })
                });
                delivered.push(envelope);
                false
            }
            Some(_) => true, // still running: the loop exit decides on the deadline
            None => false,
        });
        if pending.is_empty() || start.elapsed() >= deadline {
            return (delivered, pending.into_iter().cloned().collect());
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
}

/// Extract every background job id from a hook payload, preferring the
/// documented `tool_response` slot so a delegate prompt that happens to carry a
/// `job_id` can't shadow the real handles; fall back to a whole-payload scan
/// only if that slot yields none (the exact shape is not host-guaranteed).
fn extract_job_ids(payload: &serde_json::Value) -> Vec<String> {
    let ids = payload
        .get("tool_response")
        .and_then(|tr| {
            let found = find_job_ids(tr);
            (!found.is_empty()).then_some(found)
        })
        .unwrap_or_else(|| find_job_ids(payload));
    let mut seen: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        if !seen.contains(&id) {
            seen.push(id);
        }
    }
    seen
}

/// Recursively collect every job id from a hook-payload JSON, in document
/// order. A string `job_id` field is collected wherever it sits; a string that
/// is itself JSON is parsed and descended (the MCP tool result nests the
/// response envelope as a JSON-encoded string), so this stays agnostic to the
/// exact `tool_response` shape, which the host does not pin down.
fn find_job_ids(v: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_job_ids(v, &mut out);
    out
}

fn collect_job_ids(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            // A `job_id` value is the id itself, not a container to descend (and
            // not text to scan): collected once, never re-scanned as a token.
            let mut ids = Vec::new();
            for (key, value) in map {
                if key == "job_id" {
                    ids.push(value);
                } else {
                    collect_job_ids(value, out);
                }
            }
            for value in ids {
                if let serde_json::Value::String(s) = value {
                    out.push(s.clone());
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_job_ids(item, out);
            }
        }
        serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(parsed) => collect_job_ids(&parsed, out),
            // Not JSON: the prose spelling. The fan-out prose has no `job_id`
            // field at all, so its `d-<ms>-<n>` tokens are the only way those
            // jobs auto-arrive.
            Err(_) => out.extend(scan_job_ids(s)),
        },
        _ => {}
    }
}

/// Real job ids are `d-<epoch_ms>-<counter>`. Scan a plain string for such
/// tokens so a prose tool reply still yields every job of a fan-out. The shape
/// is pinned to digits so an unrelated `d-`-prefixed word never matches.
fn scan_job_ids(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|token| token_is_job_id(token))
        .map(str::to_string)
        .collect()
}

/// `d-<digits>-<digits>`, the exact [`jobs::new_job_id`] shape.
fn token_is_job_id(token: &str) -> bool {
    let mut parts = token.split('-');
    matches!(parts.next(), Some("d"))
        && parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
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

/// Owned twin of [`DelegateOpts`] for a background launch: the detached task is
/// `'static`, so it owns its inputs rather than borrowing the handler's locals.
/// Grouped so `launch_background_delegate` keeps a short signature and a fan-out
/// clones the whole set once per account instead of field by field.
#[derive(Clone)]
struct BackgroundOpts {
    prompt: std::sync::Arc<str>,
    model: Option<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    extra_args: Vec<String>,
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    resume: Option<String>,
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
    // A recognised third-party profile whose inference has nothing to
    // authenticate with would spawn a `claude` that dies on an empty envelope,
    // so refuse by name instead of spending a window on a run that cannot work.
    // The test is `has_inference_auth`, the predicate derived from
    // `build_claude_settings_json` (a validated api key, or a profile `env`
    // entry carrying `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`) — NOT the
    // usage predicate `third_party_credentialed`, whose Alibaba exemption
    // reads the console session that authenticates the quota gateway only.
    // `is_third_party` scopes the check: an OAuth account has no provider.
    if target.is_third_party() && !crate::claude::has_inference_auth(target) {
        return Err(format!("profile has no api key: {}", opts.profile));
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

/// Resolve a `profiles` fan-out list to canonical target names. Refuses by name:
/// a list over [`MAX_FANOUT`], a duplicate (case-insensitive, the same rule a
/// single `profile` resolves under), a name resolving to no account, or a
/// recognised third-party member with no inference auth source. Runs before
/// any spawn: N delegates is N real usage windows with no undo.
fn resolve_fanout(config: &AppConfig, raw: &[String]) -> std::result::Result<Vec<String>, String> {
    // An empty list passes every check below vacuously and would return a
    // success-shaped `{"jobs": []}` that spent nothing and spawned nothing.
    if raw.is_empty() {
        return Err("`profiles` is empty: name at least one profile".to_string());
    }
    if raw.len() > MAX_FANOUT {
        return Err(format!(
            "`profiles` fan-out capped at {MAX_FANOUT} names; got {}",
            raw.len()
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(raw.len());
    for name in raw {
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(format!(
                "duplicate profile in `profiles`: `{name}` (case-insensitive)"
            ));
        }
    }
    let mut resolved = Vec::with_capacity(raw.len());
    let mut missing = Vec::new();
    for name in raw {
        match config.canonical_name(name) {
            Some(canonical) => resolved.push(canonical),
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(format!("profile not found: {}", missing.join(", ")));
    }
    // A member with nothing to authenticate inference refuses the whole
    // fan-out before the first spawn, like an unknown name does: the spend has
    // no undo. Same predicate as `run_delegate`'s guard (`has_inference_auth`,
    // derived from `build_claude_settings_json`): Alibaba's console session
    // does NOT count (it authenticates the quota gateway only), and an OAuth
    // account stays outside the check (`is_third_party` scopes it to
    // providers).
    for name in &resolved {
        let profile = config
            .find(name)
            .ok_or_else(|| format!("profile not found: {name}"))?;
        if profile.is_third_party() && !crate::claude::has_inference_auth(profile) {
            return Err(format!("profile has no api key: {name}"));
        }
    }
    Ok(resolved)
}

/// Join a relative path onto `base` lexically, resolving `.` and `..` without
/// touching the filesystem. Refuses an absolute path and a `..` that escapes
/// `base`. `base` is already canonical, so the result is lexically under it;
/// the caller re-checks symlinks right before the read.
fn normalize_join(
    base: &std::path::Path,
    rel: &str,
) -> std::result::Result<std::path::PathBuf, String> {
    if std::path::Path::new(rel).is_absolute() {
        return Err(format!(
            "prompt_file `{rel}` refused: absolute path (must be relative to `cwd`)"
        ));
    }
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in std::path::Path::new(rel).components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(format!("prompt_file `{rel}` refused: path escapes `cwd`"));
                }
            }
            std::path::Component::Normal(part) => parts.push(part.to_os_string()),
            // On Windows `is_absolute()` needs BOTH a prefix and a root, so a
            // drive-relative `C:foo` and a root-relative `\foo` both pass the
            // check above and arrive here. Dropping either component silently
            // re-roots the path under `base` and reads a different file than
            // the caller named, so refuse by name.
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "prompt_file `{rel}` refused: absolute path (must be relative to `cwd`)"
                ));
            }
        }
    }
    let mut out = base.to_path_buf();
    for part in parts {
        out.push(part);
    }
    Ok(out)
}

/// Resolve and read a `prompt_file` relative to the delegate's `cwd`, validating
/// at the boundary and re-checking immediately before the read. The path is
/// canonicalized and checked against `cwd` in one place, then opened and read
/// with no work in between, so the thing checked is the thing read. Only a
/// regular file is accepted. Returns the prompt text.
fn read_prompt_file(cwd: Option<&str>, rel: &str) -> std::result::Result<String, String> {
    let base = match cwd {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::current_dir().map_err(|e| format!("cwd cannot be resolved: {e}"))?,
    };
    let base_real = std::fs::canonicalize(&base)
        .map_err(|e| format!("cwd '{}' cannot be resolved: {e}", base.display()))?;
    let candidate = normalize_join(&base_real, rel)?;
    // Re-check immediately before the read: canonicalize resolves any symlink, so
    // a link pointing outside `cwd` fails the starts_with check, and the resolved
    // path is the file opened below.
    let real = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if !real.starts_with(&base_real) {
        return Err(format!(
            "prompt_file `{rel}` refused: symlink target resolves outside `cwd`"
        ));
    }
    // Type check BEFORE the open: `metadata` is a stat and never opens the path,
    // so a FIFO is refused here instead of freezing the read-only open (which
    // blocks until a writer appears) on the server's only thread. A directory
    // used to slip through to an EISDIR-shaped refusal at read time; it is now
    // refused by type too.
    let meta = std::fs::metadata(&real).map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if !meta.is_file() {
        return Err(format!("prompt_file `{rel}` refused: not a regular file"));
    }
    let file =
        std::fs::File::open(&real).map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    // The check that binds, on the opened handle: a path swapped between the
    // stat above and the open cannot sneak a non-regular file past it.
    let meta = file
        .metadata()
        .map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if !meta.is_file() {
        return Err(format!("prompt_file `{rel}` refused: not a regular file"));
    }
    let size = meta.len();
    if size > PROMPT_FILE_CAP {
        return Err(format!(
            "prompt_file `{rel}` refused: {size} bytes over the {PROMPT_FILE_CAP} byte cap"
        ));
    }
    read_prompt_handle(file, rel)
}

/// Read the validated prompt handle with a hard byte ceiling. A file can grow
/// past the cap between the size check above and the read; `take` bounds the
/// read to cap + 1, and a read that actually hit the bound is refused by name
/// instead of silently truncating the prompt.
fn read_prompt_handle(file: std::fs::File, rel: &str) -> std::result::Result<String, String> {
    let mut reader = file.take(PROMPT_FILE_CAP + 1);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut buf)
        .map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if buf.len() > PROMPT_FILE_CAP as usize {
        return Err(format!(
            "prompt_file `{rel}` refused: grew past the {PROMPT_FILE_CAP} byte cap during the read"
        ));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Record ONE background job's `running` file and return its `(job_id,
/// started_at)` handle. This is the only fallible step of a background
/// delegate; the spawn that follows cannot fail, so a fan-out reserves every
/// job before launching any.
fn reserve_background_job(
    profile: &str,
    monitor: bool,
) -> std::result::Result<(String, u64), String> {
    let started_at = now_ms();
    let job_id = jobs::new_job_id(started_at);
    jobs::write_running(&job_id, profile, started_at, monitor)
        .map_err(|e| format!("failed to record job: {e}"))?;
    Ok((job_id, started_at))
}

/// Launch ONE background delegate on the blocking pool for the reserved
/// `(job_id, started_at)` handle. Infallible: `spawn_blocking` cannot fail, so
/// every failure path lives in [`reserve_background_job`]. `opts.prompt` is an
/// `Arc<str>` so a fan-out reads the prompt once and reuses it across N accounts.
fn launch_background_delegate(
    profile: String,
    opts: BackgroundOpts,
    job_id: String,
    started_at: u64,
) {
    let job_id_task = job_id;
    let profile_task = profile;
    tokio::task::spawn_blocking(move || {
        // Catch a panic in the detached task: the handle is dropped, so an unwind
        // would otherwise be swallowed and leave the job stuck `running` until
        // GC — the waiter would hang on its deadline. The job file is always
        // finalized, mirroring the sync contract.
        let BackgroundOpts {
            prompt,
            model,
            cwd,
            env,
            extra_args,
            timeout_secs,
            idle_secs,
            resume,
            isolation,
            depth,
        } = opts;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_delegate(DelegateOpts {
                profile: &profile_task,
                prompt: prompt.as_ref(),
                model: model.as_deref(),
                cwd: cwd.as_deref(),
                env,
                extra_args,
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

/// One-line throughput warning folded into a delegate payload's `live_usage`
/// object, or `None` when nothing is degraded or rate-limited.
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

/// How long a client may treat `server/discover` and `tools/list` as fresh. Both
/// are fixed for the process — the tool set is compile-time and the instructions
/// block is built once at startup — so a cached copy is never staler than the
/// server's own. rmcp defaults both to `0`, which makes a conforming client
/// re-fetch on every use.
const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

// `router = self.tool_router` dispatches from the stored router. Left off, the
// macro's default rebuilds `Self::tool_router()` on every call.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for ClauthServer {
    fn get_info(&self) -> ServerInfo {
        // Both of these are wrong by default. Empty capabilities make a
        // spec-compliant client (Claude Code) expose no tools at all, even
        // though the server still answers a forced `tools/list`; and rmcp's
        // default `Implementation` reads its OWN build env, so the server
        // introduces itself to every client as "rmcp".
        //
        // The protocol version stays at rmcp's default. It is only the fallback
        // for an `initialize` caller asking for a revision this SDK does not
        // know — a legacy client, which a 2026-07-28 answer would break —
        // while `server/discover` advertises the full supported set instead.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(build_instructions())
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_ttl_ms(CACHE_TTL_MS)
        // The instructions block names the operator's profiles, so a cached
        // copy must not cross an authorization context.
        .with_cache_scope(CacheScope::Private))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let result = ListToolsResult::with_all_items(self.tool_router.list_all());
        // Cache hints arrived with 2026-07-28; a legacy peer gets the old shape.
        let hinted = context
            .protocol_version()
            .is_some_and(|v| v >= ProtocolVersion::V_2026_07_28);
        Ok(if hinted {
            result
                .with_ttl_ms(CACHE_TTL_MS)
                .with_cache_scope(CacheScope::Public)
        } else {
            result
        })
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
                rank: roster_rank(name),
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
    // current-thread runtime panics right after the first reply. `enable_all`
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

#[cfg(test)]
#[path = "../../tests/inline/mcp_list_profiles_tool.rs"]
mod list_profiles_tool_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_format.rs"]
mod format_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_delegate_args.rs"]
mod delegate_args_tests;
