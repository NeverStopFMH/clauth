//! `clauth mcp` — MCP JSON-RPC 2.0 server over stdio (rmcp).
//!
//! Exposes clauth profiles to a live Claude Code session: list/usage, switch,
//! and delegate. The rest of the binary stays synchronous; [`serve`] builds a
//! scoped current-thread tokio runtime and blocks on the stdio server.
//!
//! All logging MUST go to stderr — stdout carries the JSON-RPC frame.

mod digest;
mod herdr_report;
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
use crate::profile::{AppConfig, Profile, load_config};
use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache};
use crate::profile_json::{
    ProfileWindows, oauth_windows, profile_windows, profile_windows_for, provider_label, tier_label,
};
use crate::providers::ThirdPartyStats;
use crate::runtime::{Isolation, ProfileRuntime};
use crate::usage::{UsageInfo, UsageWindow, now_epoch_secs, now_ms};
use digest::{DigestMode, DigestTracker, WatchOutcome, WatchSet};
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
/// Cap on the tail a RUNNING check carries. Far under [`PARTIAL_TEXT_CAP`]
/// because this rides a reply a model may fetch repeatedly, where the 8 KiB
/// salvage rides one terminal envelope.
const TAIL_CAP: usize = 400;
/// Throttle shared by the background heartbeat and `monitor`'s progress
/// notifications. Each heartbeat is an atomic tmp+rename (create, write,
/// rename) and token deltas arrive at tens per second, so 2 s bounds the store
/// to 0.5 writes/second/job — an order of magnitude inside a reader's own
/// cadence. Claude Code renders progress on a 700 ms throttle, so nothing
/// faster would be visible anyway.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
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

/// One roster throughput row, shared by [`throughput_warnings`] so every
/// surface describes a model the same way.
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

/// The subset of the per-model summary a roster is worth spending tokens on:
/// only models a past `delegate` found degraded or recently rate-limited. A
/// healthy row tells a picker nothing it would act on, and one operator's 19
/// healthy rows measured 31% of the whole `profiles` response.
fn throughput_warnings(profile: &str, now: i64) -> Vec<serde_json::Value> {
    crate::throughput::summary(profile, now)
        .into_iter()
        .filter(|m| m.degraded || m.rate_limited_recent)
        .map(throughput_row)
        .collect()
}

/// Fresh-from-cache 5h/7d windows for a profile. Each call re-reads the disk
/// cache (no caching across tool calls per the design). The roster's own rank
/// reads this: it asks for the two figures it sorts on, and consults the
/// third-party cache itself for an account that has no such window.
fn load_windows(name: &str) -> (Option<UsageWindow>, Option<UsageWindow>) {
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(u) => (u.five_hour, u.seven_day),
        None => (None, None),
    }
}

/// The discriminated headroom payload every MCP surface renders through
/// [`render::windows_prose`]: which cache answered and the figures it holds.
/// ONE carrier per account — an OAuth account's windows, or the balance/bars a
/// third-party account publishes in place of a window it does not have — so no
/// reply can print one account's figure twice or date it in a second place.
fn windows_payload(windows: &ProfileWindows) -> serde_json::Value {
    match windows {
        // An empty array is a missing FIGURE, the one case the prose reads as
        // `unknown`.
        ProfileWindows::Oauth { usage, .. } => serde_json::json!({
            "kind": "oauth",
            "windows": usage.as_deref().map(oauth_windows).unwrap_or_default(),
        }),
        ProfileWindows::ThirdParty { stats, .. } => serde_json::json!({
            "kind": "third_party",
            "balance": stats.as_ref().map(render::third_party_headline),
        }),
    }
}

/// [`windows_payload`] plus the age of the cache its figures came from, for a
/// reply carrying no other freshness cue — a running check's `quota`, a folded
/// live-usage clause — on a server that refreshes no cache of its own. Each
/// field is omitted when it carries no news, so an absent `stale` means false.
fn dated_windows_payload(windows: &ProfileWindows) -> serde_json::Value {
    let mut payload = windows_payload(windows);
    if let Some(age) = windows.age_secs() {
        payload["fetched_secs_ago"] = serde_json::json!(age);
    }
    if windows.stale() {
        payload["stale"] = serde_json::json!(true);
    }
    payload
}

/// [`windows_payload`] for a ROSTER row: undated, because 27 rows read one cache
/// generation and this is the reply whose own description asks the model to call
/// it before every delegate. `stale` still rides — a stale row costs a wrong
/// routing decision rather than a slow one — and because the figure it dates
/// lives in this same object, it renders beside that figure rather than beside a
/// structural none.
fn row_windows_payload(windows: &ProfileWindows) -> serde_json::Value {
    let mut payload = windows_payload(windows);
    if windows.stale() {
        payload["stale"] = serde_json::json!(true);
    }
    payload
}

/// The headroom payload for a running check's `quota`: whichever cache the
/// target's own fetch leg writes, so a third-party target answers with its own
/// figures instead of the `usage unknown` an OAuth-only read can only ever
/// produce for it.
fn quota_payload(name: &str) -> serde_json::Value {
    dated_windows_payload(&profile_windows_for(name))
}

/// One roster row for `p`, the shape both `profiles` scopes render through
/// `profile_line`. The one builder keeps the all-scope roster and the
/// session-scope row from disagreeing about what a profile is called.
fn profile_row(p: &Profile, config: &AppConfig, now: i64) -> serde_json::Value {
    let name = p.name.as_str();
    // One read of this account's own cache, feeding the one carrier its figures
    // ride in. Reading it twice (once to date the row, once for the headline)
    // cost a second parse of the same file on every row of the roster.
    let mut row = serde_json::json!({
        "name": name,
        "active": config.is_active(name),
        "provider": provider_label(p),
        "tier": tier_label(p),
        "windows": row_windows_payload(&profile_windows(p)),
    });
    // Host, not the full endpoint: every profile of one provider repeats the
    // same path, and the cost model only ever asks whether the host is
    // loopback or LAN.
    if let Some(url) = &p.base_url {
        row["host"] = serde_json::json!(render::base_url_host(url));
    }
    // Both of these are absent unless they say something. Emitted
    // unconditionally they were 39% of a 27-profile response, nearly all of it
    // `false` and rows carrying no warning.
    if crate::runtime::has_live_session(name) {
        row["has_live_session"] = serde_json::json!(true);
    }
    let warnings = throughput_warnings(name, now);
    if !warnings.is_empty() {
        row["throughput"] = serde_json::Value::Array(warnings);
    }
    // A third-party profile with no inference auth source is a delegate target
    // that refuses at the spawn gate, so this flags it before the picker spends
    // the call. `has_inference_auth` is the delegate guard's own predicate,
    // not the usage predicate `third_party_credentialed` (which wrongly
    // exempts Alibaba's console session).
    if p.is_third_party() && !crate::claude::has_inference_auth(p) {
        row["keyless"] = serde_json::json!(true);
    }
    // The other two states `preflight_target` refuses on, marked rather than
    // filtered: a silently missing row reads exactly like "that profile is
    // gone", which the unknown-`names` refusal already rejects.
    if p.is_disabled() {
        row["disabled"] = serde_json::json!(true);
    }
    if config.is_auth_broken(name) {
        row["auth_broken"] = serde_json::json!(true);
    }
    // Informational, not a refusal: clauth has no cancel gate, and a canceled
    // account still delegates on whatever the org's post-cancellation plan
    // allows. It rides here because the picker is choosing where to spend.
    // `is_canceled_cached` is the one cancellation predicate every surface
    // asks; re-deriving it off this row's own cache read would save a parse
    // and fork the answer, which is the trade the `list` table already made
    // the other way.
    if crate::profile_json::is_canceled_cached(name) {
        row["canceled"] = serde_json::json!(true);
    }
    row
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

/// A `delegate` argument/validation refusal: one `{is_error, result}` envelope
/// in one content block. Prose reads as a sentence; the payload keeps the same
/// keys as every other delegate refusal.
fn delegate_refusal(reason: &str) -> CallToolResult {
    let payload = serde_json::json!({ "is_error": true, "result": reason });
    let prose = render::delegate_refusal_prose(&payload);
    CallToolResult::error(single_block(prose))
}

/// The live-usage footer folded into a payload as data: which profile the
/// figures describe, and that profile's own headroom — an OAuth account's 5h/7d
/// share (null when uncached), or the balance a third-party account publishes in
/// place of a window it does not have — dated off the cache it was read from.
/// The throughput warning is added by the caller when the tool has one.
///
/// `windows` is `None` only when there is no profile to report on, which the
/// prose renders as a `none` rather than as a lost figure.
fn live_usage_json(profile: Option<&str>, windows: Option<&ProfileWindows>) -> serde_json::Value {
    let Some(windows) = windows else {
        return serde_json::json!({ "profile": profile });
    };
    let mut payload = dated_windows_payload(windows);
    payload["profile"] = serde_json::json!(profile);
    // The two shares an OAuth reader acts on directly, beside the window array
    // they were read from: this clause is a footer rather than a table, and 5h/7d
    // are the pools every other such figure in clauth refers to.
    if let ProfileWindows::Oauth { usage, .. } = windows {
        let usage = usage.as_deref();
        payload["5h_used_pct"] = serde_json::json!(
            usage
                .and_then(|u| u.five_hour.as_ref())
                .map(|w| w.utilization)
        );
        payload["7d_used_pct"] = serde_json::json!(
            usage
                .and_then(|u| u.seven_day.as_ref())
                .map(|w| w.utilization)
        );
    }
    payload
}

/// Collapse one reply to exactly one content block. The JSON payload is
/// internal — every renderer in `render.rs` reads it — and prose is the only
/// spelling a caller sees.
fn single_block(prose: String) -> Vec<ContentBlock> {
    vec![ContentBlock::text(prose)]
}

/// Fold the active profile's live usage into a payload, replacing the old
/// second-block footer. A non-object payload is wrapped under `result` first,
/// the same shape `fold_delegate_live_usage` uses: `serde_json`'s string-key
/// `IndexMut` auto-vivifies only `Null` and panics on every other non-object,
/// and the caller's payload must survive the fold.
///
/// The same fold is where the since-your-last-call digest belongs: beside
/// `live_usage`, under `since_your_last_call`, present only when something
/// moved since the last reply that reported one. `digest` decides whether this
/// reply reports (consuming the delta) or silently reseeds (`switch`'s own
/// write must not echo as news).
fn fold_active_live_usage(
    payload: serde_json::Value,
    config: &AppConfig,
    digest: DigestMode<'_>,
) -> serde_json::Value {
    let mut map = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_string(), other);
            map
        }
    };
    let active = config.state.active_profile.as_deref();
    let windows = active.map(profile_windows_for);
    map.insert(
        "live_usage".to_string(),
        live_usage_json(active, windows.as_ref()),
    );
    if let Some(delta) = digest.folded() {
        map.insert("since_your_last_call".to_string(), delta);
    }
    serde_json::Value::Object(map)
}

/// Which endpoint a delegate's requests actually WENT to, as the roster's own
/// host spelling — `anthropic` only for an account routing through neither an
/// `[env] ANTHROPIC_BASE_URL` nor an effective managed `base_url`. `None` when
/// clauth cannot read that account's config, which the renderer treats as
/// "cannot say" rather than as Anthropic.
///
/// The question is "where did this request go", so it reads `stored_endpoint`
/// (both sources, env first) and not `is_third_party` (which answers "is the
/// provider one clauth has a typed integration for"), not
/// `usage_cache_is_third_party` (which answers "which cache holds this
/// account's figures"), and not `Profile::is_oauth` (which reads the managed
/// field alone). All four disagree somewhere, and only this one bounds what
/// `total_cost_usd` may claim.
///
/// Name-keyed rather than threaded from a resolved `Profile`, because
/// `fold_done_envelope` holds only the job record's name and `load_profile`
/// there would take the state flock on a read-only collect path. One spelling
/// for every fold site is what keeps the blocking and collect replies from
/// disagreeing about one account.
fn target_endpoint(name: &str) -> Option<String> {
    match crate::profile::stored_endpoint(name) {
        crate::profile::StoredEndpoint::Anthropic => Some("anthropic".to_string()),
        crate::profile::StoredEndpoint::Custom(url) => {
            Some(render::base_url_host(&url).to_string())
        }
        crate::profile::StoredEndpoint::Unknown => None,
    }
}

/// Fold the target profile's live usage into a delegate envelope (the sync
/// `delegate` and `monitor` done-handoff paths share this). The
/// envelope is whatever `claude` printed, so it may be ANY json shape:
/// `parse_delegate_envelope` returns non-objects verbatim. A non-object is
/// wrapped under `result` (the documented self-report key) first — `serde_json`'s
/// string-key `IndexMut` auto-vivifies only `Null` and panics on every other
/// non-object, and the delegate's own output must survive the fold either way.
fn fold_delegate_live_usage(
    payload: serde_json::Value,
    profile: &str,
    now: i64,
    digest: DigestMode<'_>,
) -> serde_json::Value {
    let mut map = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_string(), other);
            map
        }
    };
    let windows = profile_windows_for(profile);
    let mut live = live_usage_json(Some(profile), Some(&windows));
    if let Some(endpoint) = target_endpoint(profile) {
        live["endpoint"] = serde_json::Value::String(endpoint);
    }
    if let Some(note) = throughput_note(profile, now) {
        live["throughput_warning"] = serde_json::Value::String(note);
    }
    map.insert("live_usage".to_string(), live);
    if let Some(delta) = digest.folded() {
        map.insert("since_your_last_call".to_string(), delta);
    }
    serde_json::Value::Object(map)
}

#[derive(Clone)]
pub(crate) struct ClauthServer {
    tool_router: ToolRouter<Self>,
    /// `Some` only when the serve path resolved a herdr pane: `delegate` then
    /// reports `working`/`idle` to herdr's agents panel. A server built
    /// without it is a silent no-op.
    herdr_pane: Option<herdr_report::PaneReporter>,
    /// The since-your-last-call baseline every clone shares (rmcp clones the
    /// handler per request; a per-clone baseline would report nothing
    /// forever). See `digest`.
    digest: DigestTracker,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SwitchArgs {
    /// Profile name to relink the global active credentials to.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProfilesArgs {
    /// Restrict the roster to these profiles (case-insensitive). Omit it, or
    /// pass an empty list, for every profile.
    names: Option<Vec<String>>,
    /// `all` (default): every profile. `session`: the one account this
    /// session's own credentials belong to, with `source` saying how that
    /// resolved — not always the configured active one. `names` filters the
    /// `all` scope only: it cannot combine with `session` (refused by name).
    scope: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateArgs {
    /// Which account(s) to run on, in canonical-resolved spelling: one name is
    /// a single delegate (blocking unless `background` is set); two or more is
    /// a background-only fan-out spending one usage window per account.
    profiles: Option<Vec<String>>,
    /// Prompt passed to the delegated `claude -p` session.
    prompt: Option<String>,
    /// Path (relative to `cwd`) of a file whose contents are the prompt, read
    /// once and reused across a fan-out so a long reusable prompt costs the
    /// calling model's context nothing. Exactly one of `prompt` or this one,
    /// never both and never neither: a call naming both (or neither) is refused
    /// by name.
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
    /// delegate runs on a detached task; the result arrives on its own via
    /// clauth's PostToolUse hook, and `monitor` checks, collects or stops it.
    /// Defaults to false.
    background: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct MonitorArgs {
    /// Job ids returned by `delegate({background: true})`: one id checks or
    /// collects that job, several collect in one call (one result per id, in
    /// the order given, capped at 256).
    job_ids: Option<Vec<String>>,
    /// Seconds to long-poll before returning (0..=3600, default 0 = reply
    /// instantly), clamped to 1500 on a client that cannot receive progress
    /// notifications. Exactly one mode per call: with `job_ids` this bounds the
    /// wait for a job to finish; with none it bounds the wait on clauth's own
    /// state, which is the mode `job_ids` cannot name.
    wait_secs: Option<u64>,
    /// `any` (the default) returns as soon as one named job finishes; `all`
    /// waits for the slowest. Needs `job_ids` — it orders a set of jobs, so
    /// naming it without them is refused.
    return_on: Option<String>,
    /// Stop the named jobs, keeping whatever they produced. Not available yet;
    /// passing `true` is refused rather than silently ignored.
    cancel: Option<bool>,
}

/// Which lane ends a several-ids wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnOn {
    /// The first job to finish. An orchestrator polls a fan-out to react to
    /// whichever lane lands first, and waiting for the slowest makes every
    /// reply as slow as it.
    Any,
    /// Every job, which is what a caller collecting an already-finished set
    /// wants.
    All,
}

#[tool_router]
impl ClauthServer {
    pub(crate) fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            herdr_pane: None,
            digest: DigestTracker::new(),
        }
    }

    /// Attach the pane reporter the serve path resolved at startup. Kept off
    /// `new()` so an in-process test, which builds its server directly, never
    /// inherits an ambient `HERDR_PANE_ID` and reports at the operator's live
    /// herdr socket. `tests/mcp_handshake.rs` does reach this path: it spawns
    /// the real binary, so it clears the herdr env on the child itself.
    pub(crate) fn with_herdr_pane(
        mut self,
        herdr_pane: Option<herdr_report::PaneReporter>,
    ) -> Self {
        self.herdr_pane = herdr_pane;
        self
    }

    #[tool(
        description = "Every clauth account, from disk cache: zero quota, no network. Call it \
before picking a `delegate` target, and pass `names` to re-check one account instead of the whole \
roster. `scope: \"session\"` answers which account THIS session's own credentials belong to, with \
`source` saying how that resolved; the configured active account can differ. In a row, higher \
`utilization_pct` means less headroom, and `keyless`, `disabled` or `auth_broken` appear only when \
true; each means `delegate` refuses that target."
    )]
    async fn profiles(
        &self,
        Parameters(ProfilesArgs { names, scope }): Parameters<ProfilesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        if let Some(raw) = scope.as_deref()
            && !matches!(raw, "all" | "session")
        {
            // An unrecognised scope is refused by name so a typo cannot
            // silently answer the wrong question.
            let payload = serde_json::json!({
                "ok": false,
                "reason": format!(
                    "unrecognized scope \"{raw}\": accepted \"all\" and \"session\""
                ),
            });
            let prose = render::list_profiles_prose(&payload);
            return Ok(CallToolResult::error(single_block(prose)));
        }
        if scope.as_deref() == Some("session") {
            // Cross-mode refusal, the same boundary rule as `monitor`'s
            // job/state seam: the session scope answers one account and cannot
            // be narrowed further, so a `names` list is a mistake worth naming
            // rather than silently ignoring — the all-scope arm would have
            // refused an unknown member by name.
            if let Some(names) = names.as_deref()
                && !names.is_empty()
            {
                let payload = serde_json::json!({
                    "ok": false,
                    "reason": "`names` cannot combine with `scope: \"session\"`: the session \
                               scope answers the one account this session runs on; drop `names`",
                });
                let prose = render::list_profiles_prose(&payload);
                return Ok(CallToolResult::error(single_block(prose)));
            }
            return self.profiles_session(&config);
        }
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
                    return Ok(CallToolResult::error(single_block(prose)));
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
            .map(|p| profile_row(p, &config, now))
            .collect();

        let payload = serde_json::json!({ "profiles": profiles });
        let prose = render::list_profiles_prose(&payload);
        Ok(CallToolResult::success(single_block(prose)))
    }

    /// The `scope: "session"` arm: the one row the account THIS session runs on
    /// resolves to, through the same `which::resolve_active` tiers the session
    /// itself resolves by, plus `source`. Rendered through `profile_line` so the
    /// row carries the roster's own guards (the anthropic tier guard included).
    fn profiles_session(&self, config: &AppConfig) -> Result<CallToolResult, ErrorData> {
        let resolved = crate::which::resolve_active(config);
        let mut rows = Vec::with_capacity(1);
        if let Some((name, source)) = resolved.as_ref()
            && let Some(p) = config.find(name)
        {
            let mut row = profile_row(p, config, now_epoch_secs());
            row["source"] = serde_json::json!(source.as_str());
            rows.push(row);
        }
        let payload = fold_active_live_usage(
            serde_json::json!({ "scope": "session", "profiles": rows }),
            config,
            DigestMode::Report(&self.digest),
        );
        let mut prose = render::list_profiles_prose(&payload);
        // Session facts ride this reply through the same renderers the
        // instructions block uses (placement rule 3: one renderer, two
        // carriers), so a client that drops the block still sees them.
        let auth = crate::which::session_auth();
        prose.push_str("\n\n");
        prose.push_str(&render::switch_effect_note(&auth));
        if let Some(note) = render::runtime_paths_note(&auth) {
            prose.push_str("\n\n");
            prose.push_str(&note);
        }
        Ok(CallToolResult::success(single_block(prose)))
    }

    #[tool(
        description = "Relink the global `~/.claude` credentials to another account. Whether THIS \
session follows depends on how it reads credentials: the reply says which case it is in, and \
`profiles({scope:\"session\"})` says so before you commit. To use another account without \
disturbing this session, use `delegate`."
    )]
    async fn switch_profile(
        &self,
        Parameters(SwitchArgs { name }): Parameters<SwitchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        // The reply's session-effect note, resolved once: `session_auth` reads
        // the env this server was launched with, which no arm below can move.
        let session_note = render::switch_effect_note(&crate::which::session_auth());

        // Resolve the raw tool argument to a stored profile (case-insensitive)
        // BEFORE any mutation — the same guard the CLI applies. Skipping it lets an
        // unknown/wrong-case name reach `link_profile_credentials`, which strips the
        // live `.credentials.json` symlink and creates no replacement (it only errors
        // later at `finish_switch`), leaving the global session credential-less.
        let Some(name) = config.canonical_name(&name) else {
            let payload =
                serde_json::json!({ "ok": false, "reason": format!("profile not found: {name}") });
            // Refused before any mutation ran, so nothing of ours moved: this
            // arm reports like the session-scope roster does. The
            // post-mutation arms below reseed instead.
            let payload =
                fold_active_live_usage(payload, &config, DigestMode::Report(&self.digest));
            let mut prose = render::switch_prose(&payload);
            prose.push_str("\n\n");
            prose.push_str(&session_note);
            return Ok(CallToolResult::error(single_block(prose)));
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
                // The mutation ran: reseed silently. The reply's own
                // `previous`/`active` is the report of what this switch did —
                // reporting its write as `since_your_last_call` news from
                // elsewhere would be a false attribution, and leaving the
                // baseline stale would echo it on the next call instead.
                let payload = fold_active_live_usage(
                    serde_json::json!({
                        "ok": true,
                        "previous": previous,
                        "active": active,
                    }),
                    &config,
                    DigestMode::Reseed(&self.digest),
                );
                let mut prose = render::switch_prose(&payload);
                prose.push_str("\n\n");
                prose.push_str(&session_note);
                Ok(CallToolResult::success(single_block(prose)))
            }
            Err(e) => {
                // Failed AFTER the mutation ran, so it may have written on the
                // way out (a stripped or repointed link): same reseed, so a
                // partial write of ours never surfaces as external news.
                let payload = fold_active_live_usage(
                    serde_json::json!({ "ok": false, "reason": e.to_string() }),
                    &config,
                    DigestMode::Reseed(&self.digest),
                );
                let mut prose = render::switch_prose(&payload);
                prose.push_str("\n\n");
                prose.push_str(&session_note);
                Ok(CallToolResult::error(single_block(prose)))
            }
        }
    }

    #[tool(
        description = "Run a task on another clauth account: a fresh headless `claude` session \
under that account's credentials. It spends that account's window or money, so pick the target \
from `profiles`. The delegate sees only `prompt` and nothing of this conversation, so state the \
whole task there, and spot-verify its `result` like any subagent's.\n\n\
Cost by target: an account with no `host` burns that subscription's 5h window; DeepSeek or Z.ai \
bills real money; Alibaba Model Studio draws down a prepaid plan; a loopback or LAN host is \
free.\n\n\
`background: true` returns a `{job_id}` now and the result arrives on its own; prefer it for a \
slow or third-party target. Two or more `profiles` always run background, one window spent per \
account. Check, collect or stop a job with `monitor`.\n\n\
`isolated: true` for a one-shot: no operator `CLAUDE.md`, plugins, hooks, skills or MCP servers, \
so it bills fewer tokens. Leave it false when the task needs this repo's tools. Either way the \
delegate loads the project `CLAUDE.md` of `cwd`, so point `cwd` at a clean dir for an unrelated \
one-shot.\n\n\
A run silent for `idle_secs`, or past the `timeout_secs` wall clock, is killed and hands back the \
text it had plus a `session_id` to `resume`, rather than paying for that work twice."
    )]
    async fn delegate(
        &self,
        Parameters(DelegateArgs {
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
            // The refusal fires before target validation, but the caller's own
            // spelling is known here: name the targets it asked for. `profiles`
            // is an optional key, present only when the caller named one.
            let payload = match &profiles {
                Some(names) => serde_json::json!({
                    "profiles": names,
                    "is_error": true,
                    "result": "delegation depth exceeded (max 1)",
                }),
                None => serde_json::json!({
                    "is_error": true,
                    "result": "delegation depth exceeded (max 1)",
                }),
            };
            let prose = render::delegate_refusal_prose(&payload);
            return Ok(CallToolResult::error(single_block(prose)));
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
            return Ok(delegate_refusal(reason));
        }

        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Which accounts to spend, in canonical spelling, resolved BEFORE any
        // spawn or read. One name is one target (blocking unless `background`);
        // a fan-out is background-only — blocking N accounts has no sensible
        // timeout story, so it is refused by name here.
        enum Target {
            One(String),
            Many(Vec<String>),
        }
        let raw: Vec<String> = profiles.unwrap_or_default();
        let target = if raw.len() == 1 {
            let Some(name) = config.canonical_name(&raw[0]) else {
                return Ok(delegate_refusal(&format!("profile not found: {}", raw[0])));
            };
            Target::One(name)
        } else if raw.is_empty() {
            return Ok(delegate_refusal(
                "`profiles` is empty: name at least one profile",
            ));
        } else if !background.unwrap_or(false) {
            return Ok(delegate_refusal(
                "`profiles` requires `background: true` for a fan-out",
            ));
        } else {
            match resolve_fanout(&config, &raw) {
                Ok(names) => Target::Many(names),
                Err(reason) => return Ok(delegate_refusal(&reason)),
            }
        };

        // Resolve the prompt text once, before any spawn, so a fan-out reuses one
        // read across every account.
        let prompt: std::sync::Arc<str> = match prompt_file.as_deref() {
            Some(rel) => match read_prompt_file(cwd.as_deref(), rel) {
                Ok(text) => text.into(),
                Err(reason) => return Ok(delegate_refusal(&reason)),
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
                    // Refuse a target `delegate` must not spend on BEFORE the
                    // job file is reserved: the caller gets the refusal
                    // synchronously, never a running job whose collected result
                    // carries it. The blocking path runs the same three gates
                    // inside `run_delegate`; `resolve_fanout` runs them per
                    // fan-out member.
                    let target = config.find(&name).ok_or_else(|| {
                        ErrorData::internal_error(
                            "resolved target missing from config".to_string(),
                            None,
                        )
                    })?;
                    if let Err(reason) = preflight_target(target, &config, &name) {
                        return Ok(delegate_refusal(&reason));
                    }
                    let extra_args = args.unwrap_or_default();
                    let streaming = !sets_output_format(&extra_args);
                    let opts = BackgroundOpts {
                        prompt,
                        model,
                        cwd,
                        env: env.unwrap_or_default(),
                        extra_args,
                        timeout_secs,
                        idle_secs,
                        resume,
                        isolation,
                        depth,
                    };
                    let spec = reserve_background_job(&name, timeout_secs, idle_secs, streaming)
                        .map_err(|e| ErrorData::internal_error(e, None))?;
                    let job_id = spec.job_id.clone();
                    let started_at = spec.started_at;
                    // Commits to launch: the job file is reserved and the task
                    // spawns next. `begin` reports `working` on the 0→1
                    // transition; each task's end-guard decrements, and the
                    // last one reports `idle`.
                    if let Some(pane) = &self.herdr_pane {
                        pane.begin();
                    }
                    launch_background_delegate(name.clone(), opts, spec, self.herdr_pane.clone());
                    // The handle carries the same footer the blocking reply
                    // does: the tool description steers a slow or third-party
                    // target here, so the recommended path must not be the one
                    // that never hears what it just spent.
                    let payload = fold_delegate_live_usage(
                        serde_json::json!({
                            "job_id": job_id,
                            "profile": name,
                            "started_at": started_at,
                            "status": "running",
                        }),
                        &name,
                        now_epoch_secs(),
                        DigestMode::Report(&self.digest),
                    );
                    let prose = render::delegate_prose(&payload);
                    return Ok(CallToolResult::success(single_block(prose)));
                }
                Target::Many(names) => {
                    let extra_args = args.unwrap_or_default();
                    let streaming = !sets_output_format(&extra_args);
                    let opts = BackgroundOpts {
                        prompt,
                        model,
                        cwd,
                        env: env.unwrap_or_default(),
                        extra_args,
                        timeout_secs,
                        idle_secs,
                        resume,
                        isolation,
                        depth,
                    };
                    // Reserve every job file BEFORE the first spawn: the reserve
                    // is the only fallible step left here (ENOSPC / perms on the
                    // jobs dir; the target pre-flight already ran in
                    // `resolve_fanout`), so a failure spends no window and loses
                    // no job id. The ids already reserved exist nowhere else;
                    // drop them and keep the all-or-nothing contract.
                    let mut specs = Vec::with_capacity(names.len());
                    for name in &names {
                        match reserve_background_job(name, timeout_secs, idle_secs, streaming) {
                            Ok(spec) => specs.push(spec),
                            Err(reason) => {
                                for spec in &specs {
                                    jobs::remove(&spec.job_id);
                                }
                                return Ok(delegate_refusal(&reason));
                            }
                        }
                    }
                    let now = now_epoch_secs();
                    let mut jobs = Vec::with_capacity(names.len());
                    for (name, spec) in names.iter().zip(specs) {
                        if let Some(pane) = &self.herdr_pane {
                            pane.begin();
                        }
                        let job_id = spec.job_id.clone();
                        let started_at = spec.started_at;
                        launch_background_delegate(
                            name.clone(),
                            opts.clone(),
                            spec,
                            self.herdr_pane.clone(),
                        );
                        // Each row carries its OWN target's headroom: the
                        // caller just spent one window per account and decides
                        // per account. `Skip`, never `Report` — reporting
                        // CONSUMES the delta, so a per-row digest would spend
                        // it N times and echo it N times.
                        jobs.push(fold_delegate_live_usage(
                            serde_json::json!({
                                "job_id": job_id,
                                "profile": name,
                                "started_at": started_at,
                                "status": "running",
                            }),
                            name,
                            now,
                            DigestMode::Skip,
                        ));
                    }
                    let mut payload = serde_json::json!({ "jobs": jobs });
                    // One digest for the whole call, top-level beside `jobs`,
                    // exactly where the batch collect path carries its own.
                    if let Some(delta) = DigestMode::Report(&self.digest).folded() {
                        payload["since_your_last_call"] = delta;
                    }
                    let prose = render::delegate_fanout_prose(&payload);
                    return Ok(CallToolResult::success(single_block(prose)));
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
        // Commits to spawn: from here the delegate is in flight. The guard
        // reports `working` to herdr's agents panel; its drop reports `idle`
        // on every exit path — clean result, deadline kill, non-zero exit,
        // unparseable output, or a task panic.
        let _pane_guard = self
            .herdr_pane
            .as_ref()
            .map(herdr_report::InFlightGuard::begin);
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
                // A blocking delegate has no job file, so it never heartbeats.
                job: None,
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

        let payload = fold_delegate_live_usage(
            envelope,
            &target,
            now_epoch_secs(),
            DigestMode::Report(&self.digest),
        );
        let is_error = payload
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let prose = render::delegate_prose(&payload);
        if is_error {
            Ok(CallToolResult::error(single_block(prose)))
        } else {
            Ok(CallToolResult::success(single_block(prose)))
        }
    }

    #[tool(
        description = "Check, collect or stop a backgrounded `delegate`, or wait on clauth's own \
state. A running job reports its account, elapsed time, how long until each deadline kills it, and \
its latest output, so a check is worth the turn it costs; a finished one returns the delegate \
envelope. `wait_secs` blocks until one named job finishes, or until clauth's state moves when you \
name none. `return_on: \"all\"` waits for the slowest job instead of the first. `cancel: true` \
stops the named jobs and keeps whatever they produced."
    )]
    async fn monitor(
        &self,
        Parameters(args): Parameters<MonitorArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.monitor_with(args, ProgressSink::from_context(&ctx))
            .await
    }

    /// The whole of `monitor`, minus the peer. Split out because an in-process
    /// caller cannot construct a `Peer<RoleServer>` — that is every test call
    /// site — and because [`ProgressSink::none`] is also exactly what a peer
    /// that sent no `progressToken` gets, so the split is a real path rather
    /// than a test-only one.
    async fn monitor_with(
        &self,
        args: MonitorArgs,
        mut progress: ProgressSink,
    ) -> Result<CallToolResult, ErrorData> {
        let MonitorArgs {
            job_ids,
            wait_secs,
            return_on,
            cancel,
        } = args;
        // Cross-mode and bad-value refusals, by name and before any waiting: a
        // rule the server refuses by name is one the description does not have
        // to teach (placement rule 4). Same shape as the `profiles` handler's
        // `scope` refusal.
        let refuse = |reason: &str| {
            let payload = serde_json::json!({ "is_error": true, "result": reason });
            Ok(CallToolResult::error(single_block(
                render::delegate_result_prose(&payload),
            )))
        };
        if cancel == Some(true) {
            // rmcp deserializes tool args with a plain `from_value` and no
            // `deny_unknown_fields`, so before this parameter existed a
            // `cancel: true` the description itself teaches was dropped and the
            // call answered as a plain check.
            return refuse(
                "`cancel` is not available yet: stopping a running delegate ships in a later \
                 release; drop it to check or collect the named jobs",
            );
        }
        let return_on = match (return_on.as_deref(), job_ids.is_some()) {
            (None, _) => ReturnOn::Any,
            (Some(_), false) => {
                return refuse(
                    "`return_on` cannot combine with the state-waiting mode: it orders a set of \
                     jobs, so name `job_ids` or drop it",
                );
            }
            (Some("any"), true) => ReturnOn::Any,
            (Some("all"), true) => ReturnOn::All,
            (Some(raw), true) => {
                return refuse(&format!(
                    "unrecognized return_on \"{raw}\": accepted \"any\" and \"all\""
                ));
            }
        };
        let wait = clamp_wait(wait_secs, progress.can_receive_progress());
        match job_ids {
            // One id keeps the single-job reply shape; several collect as a
            // batch. An empty list is job mode with no ids, refused below.
            Some(ids) if ids.len() == 1 => {
                monitor_one(
                    ids.into_iter().next().unwrap_or_default(),
                    wait,
                    &self.digest,
                    &mut progress,
                )
                .await
            }
            Some(ids) => monitor_batch(ids, wait, return_on, &self.digest, &mut progress).await,
            // No ids: the state-waiting mode absorbed from the old `watch`
            // tool — the same digest, all three observables, no filter.
            None => {
                let outcome = self.digest.watch(WatchSet::ALL, wait, &mut progress).await;
                let payload = match outcome {
                    WatchOutcome::Armed => serde_json::json!({ "status": "armed" }),
                    WatchOutcome::Unchanged { waited_secs } => {
                        serde_json::json!({ "status": "unchanged", "waited_secs": waited_secs })
                    }
                    WatchOutcome::Changed(delta) => serde_json::json!({
                        "status": "changed",
                        "since_your_last_call": delta.to_json(),
                    }),
                };
                let prose = render::watch_prose(&payload);
                Ok(CallToolResult::success(single_block(prose)))
            }
        }
    }
}

/// Env var carrying the MCP delegation depth; the child `claude` inherits
/// `depth+1` so a delegate cannot itself delegate (hard cap at 1).
const MCP_DEPTH_ENV: &str = "CLAUTH_MCP_DEPTH";

/// Poll interval mirroring `start.rs`'s `wait_for_child` cadence.
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Ceiling on `monitor`'s long-poll wait (seconds), matching
/// [`MAX_RUN_TIMEOUT_SECS`] so one call can cover any delegate. One tool, one
/// `wait_secs` parameter, so both waiting modes share one ceiling: a tool cannot
/// carry two limits on one parameter name.
const MAX_WAIT_SECS: u64 = MAX_RUN_TIMEOUT_SECS;
/// Ceiling for a peer that supplied no `progressToken`. The 3600 s cap above
/// depends on progress notifications re-anchoring Claude Code's 30-minute stdio
/// idle abort; a peer that sent no token cannot receive them, and the unclamped
/// cap would turn every long wait into a hard abort. The token IS the capability
/// probe — a config key would ask the operator to know their client's
/// idle-timeout behaviour, which is precisely the thing they cannot observe.
const MAX_WAIT_SECS_NO_PROGRESS: u64 = 1500;

/// The wait this call actually gets: the requested seconds under whichever
/// ceiling this peer can survive.
fn clamp_wait(wait_secs: Option<u64>, can_receive_progress: bool) -> u64 {
    let cap = if can_receive_progress {
        MAX_WAIT_SECS
    } else {
        MAX_WAIT_SECS_NO_PROGRESS
    };
    wait_secs.unwrap_or(0).min(cap)
}

/// Everything one `monitor` call needs from its own request: the peer plus the
/// progress token it supplied, the throttle clock and monotonic counter those
/// need (rmcp's `progress` field must strictly increase across one request's
/// notifications), and the cancellation token every wait loop races its sleep
/// against.
///
/// The cancel token lives here because this is already the one value threaded
/// through all three loops, and because it is the half of `RequestContext` a
/// test can construct — a `Peer<RoleServer>` is not.
///
/// A notification is best-effort. A dropped transport ends the request anyway,
/// and a failed one must never fail the wait it was describing.
pub(crate) struct ProgressSink {
    channel: Option<(rmcp::Peer<RoleServer>, rmcp::model::ProgressToken)>,
    /// Fired when the client sends `notifications/cancelled` for this request.
    /// rmcp cancels it but awaits the handler future bare, so nothing ends the
    /// call unless a loop reads this.
    ct: tokio_util::sync::CancellationToken,
    sent: f64,
    last: Option<Instant>,
}

impl ProgressSink {
    /// A sink with no channel — the same state [`Self::from_context`] builds
    /// for a peer that sent no `progressToken`, reachable directly so an
    /// in-process caller, which cannot construct a `Peer<RoleServer>`, still
    /// drives the real handler.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            channel: None,
            ct: tokio_util::sync::CancellationToken::new(),
            sent: 0.0,
            last: None,
        }
    }

    /// This call's cancellation token, for a test that needs to fire it. The
    /// real one arrives on the request.
    #[cfg(test)]
    pub(crate) fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.ct.clone()
    }

    fn from_context(ctx: &RequestContext<RoleServer>) -> Self {
        Self {
            channel: ctx
                .meta
                .get_progress_token()
                .map(|token| (ctx.peer.clone(), token)),
            ct: ctx.ct.clone(),
            sent: 0.0,
            last: None,
        }
    }

    /// Sleep one poll slice, or wake the moment the client abandons the call.
    /// `true` = cancelled, which every loop treats as its deadline arriving:
    /// the response is discarded either way, so the cheapest correct thing is
    /// to stop reading disk and stop notifying a request id that is gone.
    async fn sleep_or_cancelled(&self, slice: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(slice) => false,
            () = self.ct.cancelled() => true,
        }
    }

    /// Whether this peer can receive progress at all, which is what decides the
    /// wait ceiling ([`clamp_wait`]).
    fn can_receive_progress(&self) -> bool {
        self.channel.is_some()
    }

    /// Send one progress line, at most once per [`HEARTBEAT_INTERVAL`]. The
    /// message is built lazily so a throttled tick costs nothing.
    async fn tick(&mut self, message: impl FnOnce() -> String) {
        let now = Instant::now();
        if self.channel.is_none()
            || self
                .last
                .is_some_and(|t| now.duration_since(t) < HEARTBEAT_INTERVAL)
        {
            return;
        }
        self.last = Some(now);
        self.sent += 1.0;
        if let Some((peer, token)) = self.channel.as_ref() {
            let param = rmcp::model::ProgressNotificationParam::new(token.clone(), self.sent)
                .with_message(message());
            let _ = peer.notify_progress(param).await;
        }
    }
}
/// Poll cadence for both `monitor` modes and the `mcp-await-job` hook.
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Self-deadline for the `mcp-await-job` hook: outlast the max delegate timeout
/// plus slack so it never gives up before a legitimately long delegate finishes.
const AWAIT_JOB_DEADLINE_SECS: u64 = MAX_RUN_TIMEOUT_SECS + 600;

/// The one-id half of `monitor`'s job mode, byte-compatible with the
/// pre-merge single-`job_id` spelling: one envelope/status/error in one block,
/// an unknown or unsafe id refused by name.
async fn monitor_one(
    job_id: String,
    wait: u64,
    digest: &DigestTracker,
    progress: &mut ProgressSink,
) -> Result<CallToolResult, ErrorData> {
    if !jobs::is_safe_job_id(&job_id) {
        let payload = serde_json::json!({ "is_error": true, "result": "invalid job_id" });
        let prose = render::delegate_result_prose(&payload);
        return Ok(CallToolResult::error(single_block(prose)));
    }
    // A collect is the other moment a corpse matters: a server that died
    // mid-job leaves a file polling `running` forever, and `RUNNING_TTL_MS`
    // already knows it is one. Corpses only — a reader that swept `done` files
    // would delete the very envelope this call came for.
    jobs::gc_running_corpses(now_ms());
    let outcome = wait_for_done(&job_id, wait, progress).await;

    match outcome {
        WaitOutcome::Unknown => {
            let payload = serde_json::json!({
                "is_error": true,
                "result": unknown_job_reason(&job_id, now_ms()),
            });
            let prose = render::delegate_result_prose(&payload);
            Ok(CallToolResult::error(single_block(prose)))
        }
        WaitOutcome::Running(record) => {
            let payload = running_payload(&job_id, &record, now_ms());
            let prose = render::delegate_result_prose(&payload);
            Ok(CallToolResult::success(single_block(prose)))
        }
        WaitOutcome::Done(record) => {
            let (blocks, is_error) = render_done_envelope(record, digest);
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

/// The several-ids half of `monitor`'s job mode: one result per requested id
/// in the order given. An absent id is its own `unknown` result, never a
/// batch-level failure; a done id is evicted only after the whole batch
/// rendered, so a mid-fold panic leaves every done file as its recoverable
/// copy. The protocol-level error flag mirrors the per-result flags: any failed
/// done envelope makes the whole batch an error.
async fn monitor_batch(
    job_ids: Vec<String>,
    wait: u64,
    return_on: ReturnOn,
    digest: &DigestTracker,
    progress: &mut ProgressSink,
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
        return Ok(CallToolResult::error(single_block(prose)));
    }
    // An empty list passes every per-id check vacuously and would return a
    // success-shaped `{"results": []}` that collected nothing.
    if job_ids.is_empty() {
        let reason = "`job_ids` is empty: name at least one job_id";
        let payload = serde_json::json!({ "is_error": true, "result": reason });
        let prose = render::delegate_result_prose(&payload);
        return Ok(CallToolResult::error(single_block(prose)));
    }

    // Same reason as the one-id arm, and the same narrow scope.
    jobs::gc_running_corpses(now_ms());
    let outcomes = wait_for_batch(&job_ids, wait, return_on, progress).await;

    let mut results = Vec::with_capacity(outcomes.len());
    let mut delivered = Vec::new();
    let mut any_error = false;
    for (id, outcome) in outcomes {
        let entry = match outcome {
            WaitOutcome::Unknown => serde_json::json!({ "job_id": id, "status": "unknown" }),
            WaitOutcome::Running(record) => running_payload(&id, &record, now_ms()),
            WaitOutcome::Done(record) => {
                // No per-result digest: one rides the whole reply below.
                let (mut payload, is_error) = fold_done_envelope(&record, DigestMode::Skip);
                any_error |= is_error;
                // The folded envelope is always an object (a non-object
                // self-report is wrapped under `result` first), so the caller's
                // per-id markers cannot collide with delegate output.
                if let serde_json::Value::Object(map) = &mut payload {
                    map.insert("job_id".to_string(), serde_json::Value::String(id.clone()));
                    map.insert(
                        "status".to_string(),
                        serde_json::Value::String("done".to_string()),
                    );
                }
                // Evict only when the file self-reports the id it was fetched
                // under, and evict by that caller-supplied id, never the
                // stored one: `jobs::remove` joins the id into a path without
                // a safety check, so a mismatched self-report (a hand-written
                // file) must never pick the eviction path.
                if record.job_id == id {
                    delivered.push(id);
                }
                payload
            }
        };
        results.push(entry);
    }
    let mut payload = serde_json::json!({ "results": results });
    // One digest for the whole call, top-level beside `results` where every
    // other surface carries it: a batch IS one call, and a copy folded into
    // each done result would consume the change into a place the prose
    // spelling — the default one — never renders.
    if let Some(delta) = DigestMode::Report(digest).folded() {
        payload["since_your_last_call"] = delta;
    }
    let prose = render::delegate_result_batch_prose(&payload);
    let blocks = single_block(prose);
    for id in delivered {
        jobs::remove(&id);
    }
    // The batch-level error flag mirrors the per-result flags: any failed
    // delegate makes the whole batch an error, so a client branching on
    // `isError` reads a failed job the same way in both spellings.
    if any_error {
        Ok(CallToolResult::error(blocks))
    } else {
        Ok(CallToolResult::success(blocks))
    }
}

/// Fold a finished job's envelope the way every delivery path does, returning
/// the payload and its error flag. Pure of the job store: the caller evicts the
/// file only after its render, so a panic inside leaves the job file as the
/// recoverable copy of the delegate's result.
fn fold_done_envelope(
    record: &jobs::JobRecord,
    digest: DigestMode<'_>,
) -> (serde_json::Value, bool) {
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
        digest,
    );
    let is_error = payload
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (payload, is_error)
}

/// Render a finished job's envelope into its response blocks and error flag.
fn render_done_envelope(
    record: jobs::JobRecord,
    digest: &DigestTracker,
) -> (Vec<ContentBlock>, bool) {
    let (payload, is_error) = fold_done_envelope(&record, DigestMode::Report(digest));
    let prose = render::delegate_result_prose(&payload);
    (single_block(prose), is_error)
}

/// Result of polling a background job file.
enum WaitOutcome {
    Done(jobs::JobRecord),
    /// Present but not yet finished (the wait deadline elapsed first). Carries the
    /// record so the caller can report `elapsed_secs`.
    Running(jobs::JobRecord),
    /// No such job file (never created or already evicted).
    Unknown,
}

/// The running-check payload both `monitor` arms render, so the one-id and
/// several-ids spellings cannot drift. `now` is epoch ms.
///
/// A field clauth structurally cannot have is ABSENT rather than `unknown`: no
/// `last_output_secs_ago` before the first line arrives, no `idle_kill_in_secs`
/// when the idle leg is off, no tail when there is none. A record carrying no
/// `timeout_secs` predates the fields entirely (`resolve_deadlines` clamps a
/// real one to 1..=3600), so the whole liveness set is dropped rather than
/// counted down from a defaulted zero.
///
/// Every figure here is one epoch-ms subtraction, and the only inaccuracy is the
/// heartbeat throttle: the file is rewritten at most once per
/// [`HEARTBEAT_INTERVAL`], so `last_output_secs_ago` can over-report silence by
/// up to that interval and each countdown under-report by the same. The kill
/// path itself does not read this file — it reads the in-process `progress`
/// atomic — so the two never have to agree exactly.
fn running_payload(job_id: &str, record: &jobs::JobRecord, now: u64) -> serde_json::Value {
    let elapsed_ms = now.saturating_sub(record.started_at);
    let elapsed_secs = elapsed_ms / 1000;
    let mut payload = serde_json::json!({
        "job_id": job_id,
        "status": "running",
        "profile": record.profile,
        "elapsed_secs": elapsed_secs,
        "quota": quota_payload(&record.profile),
    });
    if record.timeout_secs == 0 {
        return payload;
    }
    payload["wall_kill_in_secs"] =
        serde_json::json!(record.timeout_secs.saturating_sub(elapsed_secs));
    // A run that has said nothing has been idle for its whole life, which is
    // also how the kill path counts it.
    let idle_for_secs = if record.last_output_at == 0 {
        elapsed_secs
    } else {
        now.saturating_sub(record.last_output_at) / 1000
    };
    if record.last_output_at > 0 {
        payload["last_output_secs_ago"] = serde_json::json!(idle_for_secs);
    }
    if let Some(idle) = record.idle_secs {
        payload["idle_kill_in_secs"] = serde_json::json!(idle.saturating_sub(idle_for_secs));
    }
    if !record.tail.is_empty() {
        payload["tail"] = serde_json::json!(record.tail);
    }
    payload
}

/// The epoch-ms a job id carries in its own mint shape, `None` for a token
/// clauth never minted.
fn job_id_minted_at(token: &str) -> Option<u64> {
    token_is_job_id(token)
        .then(|| token.split('-').nth(1))
        .flatten()
        .and_then(|ms| ms.parse().ok())
}

/// Why an id names no job file, and what the caller can do about it.
///
/// Only the FIRST branch is a derivation: a token off the mint shape was never a
/// clauth job at all. The mint stamp the id carries bounds a job's age and
/// nothing more — it cannot say which of the other three causes fired, since a
/// job minted two hours ago may equally have been collected five minutes ago —
/// so the age branch names the sweep as the likeliest cause and hedges like the
/// branch below it, rather than asserting a cause and telling the caller to
/// spend another window on it. Already-collected and dropped-past-the-cap share
/// one branch because nothing on disk survives either.
fn unknown_job_reason(job_id: &str, now: u64) -> String {
    let Some(minted_at) = job_id_minted_at(job_id) else {
        return format!(
            "unknown job_id: {job_id} — clauth never minted it (a real one reads \
             `d-<epoch_ms>-<counter>`); check the id `delegate` handed back"
        );
    };
    let collected = "already collected by an earlier `monitor` call or delivered by clauth's \
                     auto-delivery hook";
    if now.saturating_sub(minted_at) > jobs::DONE_TTL_MS {
        // Collection leads even here. Every collect evicts through
        // `jobs::remove`, while the hour-after-finish sweep runs at startup
        // alone, so on a session that has been up a while the sweep is the
        // rarer of the two rather than the likelier.
        return format!(
            "unknown job_id: {job_id} — most likely {collected}; minted over an hour ago, so \
             it may also have been swept an hour after it finished. check this session's \
             earlier replies before re-running the delegate"
        );
    }
    format!(
        "unknown job_id: {job_id} — {collected}, or dropped once the store passed its {} \
         newest jobs; check this session's earlier replies for the result",
        jobs::MAX_RETAINED
    )
}

/// Poll a job file until it reports `done`, `deadline_secs` elapses, or the
/// client abandons the call, ticking progress each slice off the freshest
/// running record. `Unknown` when the file is absent (distinct from `Running`
/// for a present-but-incomplete job).
async fn wait_for_done(
    job_id: &str,
    deadline_secs: u64,
    progress: &mut ProgressSink,
) -> WaitOutcome {
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    let mut cancelled = false;
    loop {
        match jobs::read(job_id) {
            Some(r) if r.state == jobs::JobState::Done => return WaitOutcome::Done(r),
            Some(r) if cancelled || start.elapsed() >= deadline => {
                return WaitOutcome::Running(r);
            }
            Some(r) => {
                progress
                    .tick(|| render::running_status_prose(&running_payload(job_id, &r, now_ms())))
                    .await;
            }
            None => return WaitOutcome::Unknown,
        }
        cancelled = progress.sleep_or_cancelled(JOB_POLL_INTERVAL).await;
    }
}

/// Poll every id until the wait ends, mirroring `await_job_outcomes`'s
/// semantics: a done file resolves at once, an absent file resolves at once (it
/// never appears for a caller-supplied id), and a running file holds. One
/// outcome per id, in the order given.
///
/// `ReturnOn::Any` ends the wait on the first job to finish, so the reply is not
/// paced by the slowest lane. That break leaves slots unresolved, and the
/// deadline can cross mid-pass under either mode, so a final pass resolves every
/// remaining slot by its own state. The invariant it protects: `Unknown` belongs
/// to a MISSING file only — a running id must never fall out as one.
async fn wait_for_batch(
    job_ids: &[String],
    deadline_secs: u64,
    return_on: ReturnOn,
    progress: &mut ProgressSink,
) -> Vec<(String, WaitOutcome)> {
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    // `None` = unresolved. An unsafe id can never name a job file
    // (`new_job_id` mints only safe ids), so it resolves to `Unknown` upfront
    // and never reaches the path join.
    let mut outcomes: Vec<Option<WaitOutcome>> = job_ids
        .iter()
        .map(|id| (!jobs::is_safe_job_id(id)).then_some(WaitOutcome::Unknown))
        .collect();
    let mut any_done = false;
    let mut cancelled = false;
    loop {
        let mut unresolved = false;
        let mut newest: Option<jobs::JobRecord> = None;
        for (id, slot) in job_ids.iter().zip(&mut outcomes) {
            if slot.is_some() {
                continue;
            }
            match jobs::read(id) {
                Some(r) if r.state == jobs::JobState::Done => {
                    any_done = true;
                    *slot = Some(WaitOutcome::Done(r));
                }
                Some(r) if cancelled || start.elapsed() >= deadline => {
                    *slot = Some(WaitOutcome::Running(r));
                }
                Some(r) => {
                    unresolved = true;
                    newest = Some(r);
                }
                None => *slot = Some(WaitOutcome::Unknown),
            }
        }
        if !unresolved || (return_on == ReturnOn::Any && any_done) {
            break;
        }
        if let Some(record) = &newest {
            progress
                .tick(|| {
                    render::running_status_prose(&running_payload(&record.job_id, record, now_ms()))
                })
                .await;
        }
        cancelled = progress.sleep_or_cancelled(JOB_POLL_INTERVAL).await;
    }
    job_ids
        .iter()
        .zip(outcomes)
        .map(|(id, slot)| {
            let outcome = slot.unwrap_or_else(|| match jobs::read(id) {
                Some(r) if r.state == jobs::JobState::Done => WaitOutcome::Done(r),
                Some(r) => WaitOutcome::Running(r),
                None => WaitOutcome::Unknown,
            });
            (id.clone(), outcome)
        })
        .collect()
}

/// `clauth mcp-await-job` — the body of the bundled PostToolUse `asyncRewake`
/// hook. Reads the hook payload on stdin, finds every background `job_id` in it,
/// waits for each, prints the delivered envelopes to stdout, and exits 2 to wake
/// the model. A sync `delegate` (no `job_id` in the payload) is a no-op (exit 0).
/// On its own deadline it exits 2 with a nudge to call `monitor` instead.
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
        "delegate {noun} `{}` still running; call `monitor` to retrieve {}",
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
    /// The reserved running record this run heartbeats into. `None` for a
    /// blocking delegate, which has no job file to write.
    job: Option<jobs::RunningSpec>,
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

/// Why the supervision loop stopped waiting on the child, when it was not the
/// child exiting. Both arms leave the loop rather than returning, so the stdout
/// reader thread is joined on every path out of `run_delegate`.
enum WaitEnd {
    /// A deadline fired; the child was killed and hands back what it wrote.
    Expired(Expiry),
    /// `try_wait` itself failed, so clauth no longer knows the child's state.
    Failed(String),
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

/// The delegate's newest assistant text as one bounded display line: the last
/// [`TAIL_CAP`] bytes (on a char boundary), every whitespace run collapsed to a
/// single space and the ends trimmed. A running status is a status, and a
/// delegate's answer is full of newlines.
fn tail_line(capture: &StreamCapture) -> String {
    let mut text = capture.partial_text();
    keep_tail(&mut text, TAIL_CAP);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The background run's "write this now" callback. It is handed the capture and
/// nothing else: the run-relative clock `read_stdout` keeps is anchored at the
/// child's spawn while the job file's `started_at` is anchored at the reserve,
/// and passing one where the other is meant is the skew this signature removes.
type HeartbeatSink<'a> = &'a mut dyn FnMut(&StreamCapture);

/// Read the child's stdout to EOF, stamping `progress` with the elapsed
/// milliseconds at every line so the wait loop can tell a working delegate from
/// a stalled one. Non-streaming mode drains the pipe whole (there is nothing to
/// stamp until the child exits, and so nothing to heartbeat either).
///
/// `heartbeat` is the background run's "write this now" callback, called at most
/// once per [`HEARTBEAT_INTERVAL`]. The throttle lives HERE rather than in the
/// sink so it is testable in one place and every sink stays pure. The sink is a
/// closure on this thread rather than a read from the supervision loop because
/// the tail text lives inside `StreamCapture`, which this thread owns
/// exclusively: handing it over would mean a `Mutex<String>` written once per
/// token delta on the hottest path in the run, which is a lock the MCP layer is
/// not allowed to add and buys nothing. `None` is a blocking `delegate`, which
/// has no job file to write — which keeps this function pure under test and
/// makes "heartbeats are background-only" structural.
fn read_stdout<R: std::io::Read>(
    reader: R,
    streaming: bool,
    start: Instant,
    progress: &AtomicU64,
    mut heartbeat: Option<HeartbeatSink<'_>>,
) -> StreamCapture {
    let mut reader = reader;
    if !streaming {
        return StreamCapture::from_raw(&drain_pipe(&mut reader));
    }
    let mut buffered = std::io::BufReader::new(reader);
    let mut capture = StreamCapture::default();
    let mut raw = Vec::new();
    let mut last_beat: Option<Instant> = None;
    loop {
        raw.clear();
        // read_until over lines(): a single event can carry a multi-megabyte tool
        // result, and invalid UTF-8 must not end the capture early.
        match std::io::BufRead::read_until(&mut buffered, b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let stamp = elapsed_ms(start);
        progress.store(stamp, Ordering::Relaxed);
        capture.push_line(String::from_utf8_lossy(&raw).trim());
        if let Some(sink) = heartbeat.as_mut() {
            let now = Instant::now();
            if last_beat.is_none_or(|t| now.duration_since(t) >= HEARTBEAT_INTERVAL) {
                last_beat = Some(now);
                sink(&capture);
            }
        }
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
    // stops opening a brand-new session on one already disabled. Also the
    // backstop for a background job whose target changed after its pre-flight,
    // since the config is re-loaded here. Guard rationale: `preflight_target`.
    preflight_target(target, &config, opts.profile)?;

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
    let job = opts.job.clone();
    let stdout_reader = child.stdout.take().map(|h| {
        let progress = std::sync::Arc::clone(&progress);
        std::thread::spawn(move || match job {
            // A background run rewrites its own job file as it reads, so a
            // `monitor` check sees liveness that would otherwise die with this
            // task. Best-effort: a failed heartbeat costs one stale check, and
            // must never end the capture.
            Some(spec) => {
                let mut beat = |capture: &StreamCapture| {
                    let _ = jobs::write_heartbeat(&spec, now_ms(), &tail_line(capture));
                };
                read_stdout(h, streaming, start, &progress, Some(&mut beat))
            }
            None => read_stdout(h, streaming, start, &progress, None),
        })
    });
    let stderr_reader = child
        .stderr
        .take()
        .map(|mut h| std::thread::spawn(move || drain_pipe(&mut h)));

    let (wall, idle) = resolve_deadlines(opts.timeout_secs, opts.idle_secs, streaming);

    // Nothing between the spawn above and the join below may return: the reader
    // thread would outlive this call, the child would keep writing into it
    // (`Child::drop` does not kill), and its heartbeats would overwrite the
    // `write_done` the caller makes next — leaving a finished job polling
    // `running` until GC and an `mcp-await-job` blocked on a terminal state that
    // never arrives. So a supervision failure kills and falls through to the
    // same join every other path takes, carrying its reason.
    // `run_delegate_never_returns_between_spawning_the_reader_and_joining_it`
    // is the guard; this comment only says why it is there.
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                let last_progress = Duration::from_millis(progress.load(Ordering::Relaxed));
                if let Some(expiry) = expiry(start.elapsed(), last_progress, wall, idle, streaming)
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(WaitEnd::Expired(expiry));
                }
                std::thread::sleep(RUN_POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(WaitEnd::Failed(format!("failed to wait for claude: {e}")));
            }
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
        Err(WaitEnd::Failed(reason)) => return Err(reason),
        Err(WaitEnd::Expired(expiry)) => {
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
        // A non-zero exit can be a throttle; record it so `profiles` can flag
        // the model as rate-limited (clauth never sees inference 429s any
        // other way).
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

/// Refuse a resolved target that `delegate` must not spend on: a profile the
/// operator disabled, one whose OAuth chain is quarantined, or a recognised
/// third-party profile whose inference has nothing to authenticate with (which
/// would spawn a `claude` that dies on an empty envelope). The keyless test is
/// `has_inference_auth`, the predicate derived from
/// `build_claude_settings_json` (a validated api key, or a profile `env` entry
/// carrying `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`) — NOT the usage
/// predicate `third_party_credentialed`, whose Alibaba exemption reads the
/// console session that authenticates the quota gateway only. `is_third_party`
/// scopes the check: an OAuth account has no provider.
///
/// Every refusal names its fix, because the reader is a model that can run the
/// command: an unnamed one costs a turn to look up.
///
/// The quarantine gate is `config.is_auth_broken`, a pure in-memory read of
/// `AppState::auth_broken`, and it is deliberately NOT a refresh attempt: the
/// MCP layer takes no rotation lock. It sits AFTER the disabled bail for the
/// reason `switch` orders them the same way — a disabled, clock-expired target
/// must be refused before anything can rotate its single-use refresh token.
///
/// Called from every path that refuses before a spawn: the single-background
/// arm and `resolve_fanout` up front, and `run_delegate` as the blocking
/// path's own check plus the backstop for a target that changed after its
/// pre-flight (the config is re-loaded there).
fn preflight_target(
    profile: &Profile,
    config: &AppConfig,
    name: &str,
) -> std::result::Result<(), String> {
    if profile.is_disabled() {
        return Err(format!(
            "profile is disabled: {name} (run `clauth enable {name}`)"
        ));
    }
    // Verbatim `switch`'s own refusal (`actions.rs`, its AUTH-1 arm), so the two
    // surfaces cannot spell one quarantine two ways. It already names the fix.
    if config.is_auth_broken(name) {
        return Err(crate::format::login_expired(name).line());
    }
    if profile.is_third_party() && !crate::claude::has_inference_auth(profile) {
        // `--api-key` is what selects api-key mode; a bare `clauth login` on a
        // third-party profile runs the browser OAuth flow instead and leaves
        // the missing key missing.
        return Err(format!(
            "profile has no api key: {name} (run `clauth login {name} --api-key <key>`)"
        ));
    }
    Ok(())
}

/// Resolve a `profiles` fan-out list to canonical target names. Refuses by name:
/// a list over [`MAX_FANOUT`], a duplicate (case-insensitive, the same rule a
/// single `profile` resolves under), a name resolving to no account, a disabled
/// member, or a recognised third-party member with no inference auth source.
/// Runs before any spawn: N delegates is N real usage windows with no undo.
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
    // A member that cannot be spent on refuses the whole fan-out before the
    // first spawn, like an unknown name does: the spend has no undo. Same
    // pre-flight as the single-background arm (`preflight_target`, rationale
    // there): disabled by the operator, or a recognised third-party profile
    // with nothing to authenticate inference.
    for name in &resolved {
        let profile = config
            .find(name)
            .ok_or_else(|| format!("profile not found: {name}"))?;
        preflight_target(profile, config, name)?;
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
/// instead of silently truncating the prompt. Invalid UTF-8 is refused by name
/// at the byte offset, never lossily decoded.
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
    let text = std::str::from_utf8(&buf).map_err(|e| {
        format!(
            "prompt_file `{rel}` refused: invalid UTF-8 at byte offset {}",
            e.valid_up_to()
        )
    })?;
    Ok(text.to_string())
}

/// Record ONE background job's `running` file and return the spec every later
/// write of it goes through. This is the only fallible step left after the
/// pre-flight refusal; the spawn that follows cannot fail, so a fan-out
/// reserves every job before launching any.
///
/// The deadlines are resolved HERE so the first running record already carries
/// them and `resolve_deadlines`' streaming fork is applied exactly once.
fn reserve_background_job(
    profile: &str,
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    streaming: bool,
) -> std::result::Result<jobs::RunningSpec, String> {
    let started_at = now_ms();
    let (wall, idle) = resolve_deadlines(timeout_secs, idle_secs, streaming);
    let spec = jobs::RunningSpec {
        job_id: jobs::new_job_id(started_at),
        profile: profile.to_string(),
        started_at,
        timeout_secs: wall.as_secs(),
        // Without the event stream the idle leg is off entirely, so there is no
        // such deadline to count down to rather than an unknown one.
        idle_secs: streaming.then_some(idle.as_secs()),
    };
    jobs::write_running(&spec).map_err(|e| format!("failed to record job: {e}"))?;
    Ok(spec)
}

/// Launch ONE background delegate on the blocking pool for the reserved `spec`.
/// Infallible: `spawn_blocking` cannot fail, so every failure path lives in
/// [`reserve_background_job`]. `opts.prompt` is an `Arc<str>` so a fan-out reads
/// the prompt once and reuses it across N accounts.
fn launch_background_delegate(
    profile: String,
    opts: BackgroundOpts,
    spec: jobs::RunningSpec,
    herdr_pane: Option<herdr_report::PaneReporter>,
) {
    let profile_task = profile;
    // Registered so a test's `HomeSandbox::drop` can block on this task BEFORE
    // it clears the home override: `spawn_blocking` detaches with no handle
    // kept below, so nothing else here is joinable by a sandbox teardown. A
    // task still running when the override clears resolves the operator's
    // REAL `$HOME` (filed 2026-08-14, F1).
    #[cfg(test)]
    let done_tx = crate::testutil::register_background_task();
    tokio::task::spawn_blocking(move || {
        // Test-only: block here if a test armed the start gate, forcing this
        // task to still be in flight at the moment its `HomeSandbox` drops
        // instead of racing tokio's blocking-pool scheduler for that timing.
        #[cfg(test)]
        detach_test_gate();
        // Decrements the pane's in-flight count on every exit path, panic
        // included; the drop reports `idle` once nothing is left in flight.
        // Created first so no early return can skip it.
        let _pane_end = herdr_pane.map(herdr_report::InFlightGuard::end_only);
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
                job: Some(spec.clone()),
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
        // `run_delegate` has returned, so it has already joined the reader
        // thread: the last heartbeat strictly precedes this finalize.
        let _ = jobs::write_done(&spec.job_id, &profile_task, spec.started_at, envelope);
        // Dropped explicitly so the completion signal below is genuinely this
        // task's last action. A guard bound in the closure drops in reverse
        // declaration order, i.e. AFTER the send, which would let a test's
        // teardown clear the home override while this one still runs. Harmless
        // for THIS guard, whose report shells out and reads the process env
        // rather than clauth's override — but the registry's contract is that
        // nothing touching `$HOME` outlives the send, and a guard that silently
        // sits outside it is how that contract goes false later.
        drop(_pane_end);
        #[cfg(test)]
        let _ = done_tx.send(());
    });
}

/// Test-only start gate for the NEXT detached background task: once armed,
/// blocks that task at the top of its closure until the test releases it.
/// The only way to prove `HomeSandbox` teardown ordering without racing
/// tokio's blocking-pool scheduler — an unforced test is green by luck, since
/// the task usually hasn't even started by the time a sandbox drops. Never
/// compiled into the binary.
#[cfg(test)]
static DETACH_START_GATE: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>> =
    std::sync::Mutex::new(None);

/// Arm the gate; returns the sender the test releases it with. A single
/// global slot shared by every test, so callers must hold
/// `profile::HOME_TEST_LOCK` (via a live `HomeSandbox`) for as long as it
/// stays armed — otherwise an unrelated test's own background task could be
/// the one that gets gated.
#[cfg(test)]
fn arm_detach_gate() -> std::sync::mpsc::Sender<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    *DETACH_START_GATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
    tx
}

/// Block if a test armed [`arm_detach_gate`]; a no-op otherwise (production,
/// or a test that never arms it).
#[cfg(test)]
fn detach_test_gate() {
    let armed = DETACH_START_GATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(rx) = armed {
        let _ = rx.recv();
    }
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
            Call `profiles` for live usage figures."
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
    // Resolve the pane reporter once, at startup: the pane env is what this
    // process inherited from herdr, and a per-call re-read would race a
    // delegate with a changed environment.
    let server = ClauthServer::new().with_herdr_pane(herdr_report::PaneReporter::resolve());
    let service = server.serve(stdio()).await?;
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
#[path = "../../tests/inline/mcp_profiles_tool.rs"]
mod profiles_tool_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_format.rs"]
mod format_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_delegate_args.rs"]
mod delegate_args_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_herdr_report.rs"]
mod herdr_report_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_digest.rs"]
mod digest_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_background_sandbox.rs"]
mod background_sandbox_tests;
