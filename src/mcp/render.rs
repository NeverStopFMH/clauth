//! Pure formatters for the MCP layer: init instructions block, third-party
//! headline, and the prose spellings of each tool's JSON payload, the
//! folded-in live-usage clause included. No I/O, no locks — callers pass in
//! already-loaded cache data so these stay unit-testable.

use serde_json::Value;

use crate::format::format_pct;
use crate::providers::ThirdPartyStats;
use crate::usage::humanize_duration;
use crate::which::SessionAuth;

/// Per-profile snapshot fed to [`instructions_block`]: stable identity only (name,
/// provider, tier, base url). Volatile usage figures rot within a turn, so they are
/// served fresh per call by `profiles`, never baked into the boot-time block.
pub(crate) struct ProfileSnapshot {
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) provider: String,
    pub(crate) base_url: Option<String>,
    pub(crate) sub_type: Option<String>,
    /// Where this profile sorts in the roster. See [`RosterRank`].
    pub(crate) rank: RosterRank,
}

/// A profile's roster sort key, for ordering only.
///
/// The variants never interleave: every windowed profile outranks every wallet
/// one, which outranks every profile clauth holds no figure for. That last step
/// is what keeps "no figure" from reading as "full".
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RosterRank {
    /// Percent of this profile's best-known window still FREE.
    Window(f64),
    /// A provider reporting a wallet rather than a window. Amounts are compared
    /// only within one `currency`: ordering 1117 CNY against 31 USD needs an
    /// exchange rate clauth does not have and could not keep fresh.
    Balance { currency: String, amount: f64 },
    /// Nothing cached, or nothing a wallet could be read out of.
    Unknown,
}

/// Host (and port) of a base url. Every profile of one provider carries the same
/// endpoint path, so both the roster and `profiles` print the identifying
/// half only. Shared so the two can never disagree on what a profile's endpoint
/// is called.
pub(super) fn base_url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

/// The `[provider, tier, host]` bracket a roster line ends in. Profiles sharing
/// one bracket share a line.
fn roster_bracket(p: &ProfileSnapshot) -> String {
    let mut parts = vec![p.provider.clone()];
    if let Some(s) = &p.sub_type {
        parts.push(s.clone());
    }
    if let Some(b) = &p.base_url {
        parts.push(base_url_host(b).to_string());
    }
    format!("[{}]", parts.join(", "))
}

/// Currencies in the order the roster first meets them. Two currencies carry no
/// comparable magnitude, so their groups fall back to the order the operator's
/// own config lists them in.
fn currency_order(profiles: &[ProfileSnapshot]) -> Vec<&str> {
    let mut seen: Vec<&str> = Vec::new();
    for p in profiles {
        if let RosterRank::Balance { currency, .. } = &p.rank
            && !seen.contains(&currency.as_str())
        {
            seen.push(currency);
        }
    }
    seen
}

/// Total order over [`RosterRank`] as `(tier, currency group, negated
/// magnitude)`. Sorting ascending on it puts the freest window first and every
/// unknown last, and negating the magnitude is what makes "more left" sort
/// earlier without a second comparator.
fn sort_key(p: &ProfileSnapshot, currencies: &[&str]) -> (u8, usize, f64) {
    match &p.rank {
        RosterRank::Window(free) => (0, 0, -free),
        RosterRank::Balance { currency, amount } => (
            1,
            currencies
                .iter()
                .position(|c| *c == currency.as_str())
                .unwrap_or(usize::MAX),
            -amount,
        ),
        RosterRank::Unknown => (2, 0, 0.0),
    }
}

fn cmp_key(a: (u8, usize, f64), b: (u8, usize, f64)) -> std::cmp::Ordering {
    a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.total_cmp(&b.2))
}

/// Roster body: one line per distinct bracket, names joined, most headroom first.
/// A fleet of same-provider profiles otherwise repeats one identical endpoint on
/// every line, which is pure token cost in a block every session loads. The
/// ordering is a hint rather than a claim — it freezes at server start like the
/// rest of the roster, which is why the header calls it a snapshot.
fn roster_lines(profiles: &[ProfileSnapshot]) -> String {
    let currencies = currency_order(profiles);
    let mut groups: Vec<(String, Vec<&ProfileSnapshot>)> = Vec::new();
    for p in profiles {
        let bracket = roster_bracket(p);
        match groups.iter_mut().find(|(b, _)| *b == bracket) {
            Some((_, members)) => members.push(p),
            None => groups.push((bracket, vec![p])),
        }
    }

    // Stable sorts throughout, so config order breaks every tie. Members first,
    // then groups by their best member — which is `first()` once members are
    // sorted.
    fn best(members: &[&ProfileSnapshot], currencies: &[&str]) -> (u8, usize, f64) {
        members
            .first()
            .map_or((2, 0, 0.0), |p| sort_key(p, currencies))
    }
    for (_, members) in &mut groups {
        members.sort_by(|a, b| cmp_key(sort_key(a, &currencies), sort_key(b, &currencies)));
    }
    groups.sort_by(|a, b| cmp_key(best(&a.1, &currencies), best(&b.1, &currencies)));

    let mut out = String::new();
    for (bracket, members) in &groups {
        let names: Vec<String> = members
            .iter()
            .map(|p| {
                if p.active {
                    format!("{} (active)", p.name)
                } else {
                    p.name.clone()
                }
            })
            .collect();
        out.push_str("- ");
        out.push_str(&names.join(", "));
        out.push(' ');
        out.push_str(bracket);
        out.push('\n');
    }
    out
}

/// One-line cached headline for a third-party profile from
/// `third_party_cache.json`: non-empty bars join as `label pct%`, else the first
/// stat row that carries a value; the plan label prefixes the line when present.
/// Value-less rows (e.g. DeepSeek's `USD balance` heading) are skipped so the
/// headline never renders a dangling `label:` with nothing after it.
pub(crate) fn third_party_headline(s: &ThirdPartyStats) -> String {
    let body = if !s.bars.is_empty() {
        s.bars
            .iter()
            .map(|b| format!("{} {}", b.label, format_pct(b.pct)))
            .collect::<Vec<_>>()
            .join(", ")
    } else if let Some(row) = s.rows.iter().find(|r| !r.value.is_empty()) {
        if row.label.is_empty() {
            row.value.clone()
        } else {
            format!("{}: {}", row.label, row.value)
        }
    } else if !s.is_available {
        "unavailable".to_string()
    } else {
        String::new()
    };

    match (&s.plan, body.is_empty()) {
        (Some(plan), false) => format!("{plan}: {body}"),
        (Some(plan), true) => plan.clone(),
        (None, _) => body,
    }
}

/// What a `switch_profile` does to *this* session, keyed on how it reads its
/// credentials. A global session reads the exact file `switch_profile`
/// repoints; an isolated session (a `clauth start` runtime or a custom
/// `CLAUDE_CONFIG_DIR`) reads its own, so a switch can't disturb it. The
/// subject is the lead-in [`switch_effect_note`] adds — a client that shows
/// tool names only never sees a bare `switch`. Pure mapping — the caller
/// resolves the [`SessionAuth`].
pub(crate) fn switch_effect(auth: &SessionAuth) -> String {
    match auth {
        SessionAuth::Global => "repoints the global `~/.claude` credentials THIS \
session reads; Claude Code reloads them on its next token refresh, so this session would \
start acting as the switched profile mid-task. To use another account \
without disturbing this one, use the `delegate` tool."
            .to_string(),
        SessionAuth::IsolatedRuntime(name) => format!(
            "repoints the global `~/.claude` credentials, but THIS session runs in an \
isolated `clauth start` runtime pinned to `{name}` and is unaffected. Only a later session on \
the global credentials adopts the change."
        ),
        SessionAuth::IsolatedCustom => "repoints the global `~/.claude` credentials, but \
THIS session uses a custom `CLAUDE_CONFIG_DIR` and reads its own credentials, so it is \
unaffected. Only a later session on the global credentials adopts the change."
            .to_string(),
    }
}

/// [`switch_effect`] with its lead, in the exact shape both carriers hold it:
/// the init `instructions` block and the `switch_profile` / session-scope
/// replies (placement rule 3: one renderer, two carriers, no drift).
pub(crate) fn switch_effect_note(auth: &SessionAuth) -> String {
    format!("switch_profile & this session: {}", switch_effect(auth))
}

/// How this session's runtime tree maps onto the real global one, for the only
/// tier that has such a tree. A `clauth start` runtime looks per-profile, so a
/// model editing `CLAUDE.md` or `skills/…` under it may believe the edit is
/// scoped. The note frames the consequence rather than the transport: a write
/// under the dir reaches the global file every profile loads, directly on a
/// symlink host and through the watchdog's newer-mtime mirror on a copy host.
/// One text serves both, because pinning one mechanism is where the old note
/// went false: a copy-mode host (Windows without symlink privilege) builds the
/// tree by recursive copy, so "mostly symlinks" was false there and a
/// `readlink -f` nudge had nothing to resolve. The dropped gate-binding claim
/// fell the same way: on a copy the runtime path is not the gated path; the
/// write reaches the gated file only once the mirror lands it there.
///
/// The note names `$CLAUDE_CONFIG_DIR` rather than a constructed path: the real
/// dir carries a per-session suffix (`runtime-<sid>`, the sid being `<pid>-<seq>`),
/// so any literal spelled here would point at a directory that does not exist.
/// It also names no destination past `~/.claude/`. Whether an entry there chains
/// on somewhere else is the operator's own layout rather than anything clauth
/// builds: this box reaches `~/.agents/skills` through a `~/.claude/skills`
/// symlink the operator made, and a box without it would be told a falsehood.
///
/// `Global` has no runtime dir, and `IsolatedCustom` is a foreign
/// `CLAUDE_CONFIG_DIR` whose layout clauth does not own, so neither may claim
/// this layout. Pure mapping; the caller resolves the [`SessionAuth`].
pub(crate) fn runtime_paths_note(auth: &SessionAuth) -> Option<String> {
    match auth {
        SessionAuth::IsolatedRuntime(name) => Some(format!(
            "runtime paths: this session's config dir (`$CLAUDE_CONFIG_DIR`, profile `{name}`) \
mirrors the global `~/.claude/<same-name>`: symlinks onto it where the host allows them, a \
recursive copy the watchdog reconciles where it does not. Only `.claude.json`, `settings.json` \
and `.credentials.json` are per-profile. So a write under that dir reaches the global file every \
profile and every future session loads. It lands directly on a symlink host. On a copy host it \
lands via the watchdog's newer-mtime mirror, at its sync cadence."
        )),
        SessionAuth::Global | SessionAuth::IsolatedCustom => None,
    }
}

/// Init-time `instructions` block: identity intro, a one-line tool router, a
/// session-aware `switch_profile` note, the runtime-path note that tier earns,
/// then the grouped roster. This block is the only clauth text a session is
/// guaranteed to hold: tool descriptions are deferred in some harnesses and
/// unloaded until searched for, so the router line stays even though every tool
/// carries its own description. Per-tool mechanics do NOT stay — they live in
/// that tool's description, which is loaded by the time anyone can call it, and
/// so does the `delegate` cost model. No usage percentage or reset timer is
/// baked in; those rot within a turn, so they live in `profiles`.
pub(crate) fn instructions_block(profiles: &[ProfileSnapshot], auth: &SessionAuth) -> String {
    let mut out = String::new();
    out.push_str(
        "clauth manages multiple Claude Code accounts (\"profiles\"): each an isolated \
credential set / subscription. Use its tools to compare usage headroom across accounts, relink \
the active account, or delegate a task to another account without spending this session's \
window.\n\n\
Tools: `profiles` (accounts + cached usage, zero quota; `scope:\"session\"` for this session's \
own), `switch_profile` (relink the global active account), `delegate` (run a task on another \
account; the only tool that spends), `monitor` (check, collect or stop a backgrounded delegate, \
or wait on clauth's state).\n\n\
",
    );
    out.push_str(&switch_effect_note(auth));
    if let Some(note) = runtime_paths_note(auth) {
        out.push_str("\n\n");
        out.push_str(&note);
    }
    out.push_str(
        "\n\n\
Profiles, most headroom first (session-start snapshot; call `profiles` for live usage and \
anything added since):\n",
    );
    out.push_str(&roster_lines(profiles));
    out
}

// ── prose spellings (`format: "prose"` is the default) ──────────────────────
//
// Each tool's JSON payload has exactly one prose spelling, produced here. The
// contract: prose names what carries news. A boolean flag appears as a word
// only when true (a spelled-out `false` costs tokens for nothing), telemetry a
// reader cannot act on (a sample count) stays in the JSON spelling, a null
// number reads as `unknown` (never `0%` or an omission a reader takes for
// `none`), and no figure appears that the payload did not have. Raw timestamps
// are named, not re-derived into `resets in N` — a derived figure is one the
// JSON did not carry.

/// A window's share as a prose clause: `12% used` for a number, `unknown` for
/// `None` (so a null reads as unknown, never as `unknown used`).
fn pct_clause(v: Option<f64>) -> String {
    v.map_or_else(
        || "unknown".to_string(),
        |p| format!("{} used", format_pct(p)),
    )
}

/// The folded-in `live_usage` object as a sentence clause. `lead` is the noun
/// for the profile it names: `active profile` for the session-scope roster and
/// `switch_profile`, `target` for `delegate`.
///
/// Three readings the clause keeps apart, because collapsing any two of them
/// tells the reader clauth lost something it holds: no profile at all reads
/// `none` and names no window (there is no account whose windows could be
/// reported); a third-party account has no 5h/7d pool to report, so it reads as
/// the structural none [`windows_prose`] spells; only an OAuth window with
/// nothing cached reads `unknown`.
pub(crate) fn live_usage_prose(lu: &Value, lead: &str) -> String {
    let Some(name) = lu.get("profile").and_then(Value::as_str) else {
        return format!("{lead} none");
    };
    let mut out = format!("{lead} `{name}`: ");
    if lu.get("kind").and_then(Value::as_str) == Some("third_party") {
        // Same payload keys `windows_prose` reads, so the two surfaces cannot
        // spell one account's headroom two ways.
        out.push_str(&windows_prose(lu));
    } else {
        let five = lu.get("5h_used_pct").and_then(Value::as_f64);
        let seven = lu.get("7d_used_pct").and_then(Value::as_f64);
        out.push_str(&format!(
            "5h {}, 7d {}",
            pct_clause(five),
            pct_clause(seven)
        ));
        // An age dates a FIGURE. With neither window cached there is no figure
        // to date, and stamping the cache's age onto two `unknown`s would read
        // as a measurement clauth does not have.
        if five.is_some() || seven.is_some() {
            out.push_str(&freshness_clause(lu));
        }
    }
    if let Some(w) = lu.get("throughput_warning").and_then(Value::as_str) {
        out.push_str("; ");
        out.push_str(w);
    }
    out
}

/// One profile name in the digest's from/to pair: backticked for a name,
/// `none` for a null (no active profile configured — the same read
/// [`live_usage_prose`] gives a null profile).
fn digest_name(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .map_or_else(|| "none".to_string(), |n| format!("`{n}`"))
}

/// The folded-in `since_your_last_call` object as a sentence clause: one part
/// per observable that carries news, exactly the keys the JSON spelling kept.
/// The two mtime observables have no figure a reader acts on, so their part
/// names what happened (`refreshed` / `rewritten`), never the timestamp.
pub(crate) fn digest_prose(d: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ap) = d.get("active_profile") {
        parts.push(format!(
            "active profile {} → {}",
            digest_name(ap.get("from")),
            digest_name(ap.get("to"))
        ));
    }
    if d.get("usage_cache")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("usage cache refreshed".to_string());
    }
    if d.get("credentials")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("credentials file rewritten".to_string());
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("since your last call: {}", parts.join("; "))
}

/// The state-waiting mode's reply: the change it caught, the baseline it
/// armed, or the wait that found nothing. Self-labels `monitor` — the reply
/// names the tool that can be called again, and a label naming a tool the
/// handshake does not list sends the model searching for one.
pub(crate) fn watch_prose(p: &Value) -> String {
    match p.get("status").and_then(Value::as_str) {
        Some("changed") => format!("monitor: {}", digest_prose(&p["since_your_last_call"])),
        Some("armed") => {
            "monitor armed: baseline set on this first digest call, nothing to compare against yet"
                .to_string()
        }
        _ => {
            let waited = p.get("waited_secs").and_then(Value::as_u64).unwrap_or(0);
            format!("monitor: no change after {waited}s")
        }
    }
}

/// How old the figures in a headroom payload are, and whether that is past
/// anything a live scheduler produces. Dating a figure is what lets a reader
/// discount it; suppressing a stale one would turn a known-old number into no
/// number, which reads as clauth having lost track of the account. A payload
/// carrying no age at all (the roster, which spends no tokens dating rows that
/// are current) still says `stale` when it is.
fn freshness_clause(v: &Value) -> String {
    let stale = v.get("stale").and_then(Value::as_bool).unwrap_or(false);
    let Some(secs) = v.get("fetched_secs_ago").and_then(Value::as_u64) else {
        return if stale {
            " (stale)".to_string()
        } else {
            String::new()
        };
    };
    let when = if secs == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", humanize_duration(secs as i64))
    };
    if stale {
        format!(" (cached {when}, stale)")
    } else {
        format!(" (cached {when})")
    }
}

/// The age half of [`freshness_clause`] alone, for a line whose `stale` marker
/// already renders beside the figure it dates: the age rides, the stale word
/// does not repeat.
fn age_clause(v: &Value) -> String {
    let Some(secs) = v.get("fetched_secs_ago").and_then(Value::as_u64) else {
        return String::new();
    };
    let when = if secs == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", humanize_duration(secs as i64))
    };
    format!(" (cached {when})")
}

/// The headroom clause, off the discriminated payload
/// [`crate::profile_json::ProfileWindows`] produces: an OAuth account's windows,
/// or a third-party account's own figures in place of a pool it does not draw
/// on. An OAuth account with nothing cached is the ONE case that reads
/// `unknown` — no cache is not a zero, and it is also not a window that cannot
/// exist.
///
/// The denial names ANTHROPIC's pool rather than `5h/7d` bare, because a
/// provider publishes windows under those same labels (z.ai reports `5h`, `7d`
/// and `30d`; Alibaba reports `7d`), and a clause that denies a `5h/7d` window
/// and prints one in the same breath is a contradiction the reader has to
/// resolve. `provider reports` then names whose account every following number
/// belongs to: the denial is about the Anthropic subscription pool every other
/// 5h/7d figure in clauth refers to, and the rest is this endpoint's own.
///
/// A freshness clause rides the FIGURE it dates and nothing else, on both arms:
/// stamping a cache's age onto `unknown` — or onto a structural none — asserts a
/// measurement clauth does not have.
fn windows_prose(windows: &Value) -> String {
    match windows.get("kind").and_then(Value::as_str) {
        Some("third_party") => {
            let mut out = "no Anthropic 5h/7d window".to_string();
            match windows
                .get("balance")
                .and_then(Value::as_str)
                .filter(|b| !b.is_empty())
            {
                Some(figure) => {
                    out.push_str(&format!("; provider reports {figure}"));
                    out.push_str(&freshness_clause(windows));
                }
                None => out.push_str("; provider usage unknown"),
            }
            out
        }
        Some("oauth") => {
            let ws = windows
                .get("windows")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if ws.is_empty() {
                return "usage unknown".to_string();
            }
            let mut out = ws
                .iter()
                .map(|w| {
                    let label = w.get("label").and_then(Value::as_str).unwrap_or("unknown");
                    let pct = w.get("utilization_pct").and_then(Value::as_f64);
                    let mut s = format!("{label} {}", pct_clause(pct));
                    if let Some(r) = w.get("resets_at").and_then(Value::as_str) {
                        s.push_str(&format!(" (resets at {r})"));
                    }
                    s
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&freshness_clause(windows));
            out
        }
        _ => "usage unknown".to_string(),
    }
}

/// Per-model throughput rows (`which`'s full summary or a roster's warnings).
/// A healthy row is the model's name and rate; `degraded` and the rate-limit
/// flag appear as words only when true, the retry delay with them. The sample
/// count is clauth's own confidence telemetry, not a figure a reader acts on,
/// so it stays in the JSON spelling.
fn throughput_prose(rows: &[Value]) -> String {
    rows.iter()
        .map(|m| {
            let model = m.get("model").and_then(Value::as_str).unwrap_or("unknown");
            let tok_s = m
                .get("tok_s")
                .and_then(Value::as_f64)
                .map_or_else(|| "unknown".to_string(), |v| v.to_string());
            let mut flags: Vec<String> = Vec::new();
            if m.get("degraded").and_then(Value::as_bool).unwrap_or(false) {
                flags.push("degraded".to_string());
            }
            if m.get("rate_limited_recent")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                flags.push("rate-limited recently".to_string());
                if let Some(r) = m.get("retry_after_s").and_then(Value::as_u64) {
                    flags.push(format!("retry in {r}s"));
                }
            }
            let mut s = format!("`{model}` {tok_s} tok/s");
            if !flags.is_empty() {
                s.push_str(&format!(" ({})", flags.join(", ")));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// One roster row as a prose line: name + active marker, the
/// `[provider, tier, host]` bracket, this account's own headroom, then the quiet
/// flags. A null tier reads `unknown` on an account that HAS a plan tier and
/// drops out on one that structurally has none.
///
/// The tier guard asks the headroom payload's `kind`, never the display
/// `provider`: [`crate::profile_json::provider_label`] renders every
/// unrecognised endpoint as `anthropic`, so a generic api-key account (a local
/// llama, an aggregator) would be told its Anthropic plan tier is unknown when
/// it has no Anthropic plan at all.
fn profile_line(row: &Value) -> String {
    let name = row.get("name").and_then(Value::as_str).unwrap_or("unknown");
    let active = row.get("active").and_then(Value::as_bool).unwrap_or(false);
    let provider = row
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let tier = row.get("tier").and_then(Value::as_str);
    let host = row.get("host").and_then(Value::as_str);

    let mut bracket = vec![provider.to_string()];
    if let Some(t) = tier {
        bracket.push(t.to_string());
    }
    if let Some(h) = host {
        bracket.push(h.to_string());
    }

    let mut out = format!(
        "- {}{} [{}]: {}",
        name,
        if active { " (active)" } else { "" },
        bracket.join(", "),
        windows_prose(&row["windows"]),
    );

    // A null tier is structural for an account whose usage lives in the
    // third-party cache; on a subscription account it means the plan is unknown.
    let api_key_account = row["windows"].get("kind").and_then(Value::as_str) == Some("third_party");
    if tier.is_none() && !api_key_account {
        out.push_str("; tier unknown");
    }
    if row
        .get("has_live_session")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; live session");
    }
    // The three states `delegate` refuses on render as one contiguous run, so a
    // reader meets one refusal group rather than three markers scattered
    // through the line. `canceled` follows them because clauth has no cancel
    // gate: it informs the pick, it does not block it.
    if row
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; disabled");
    }
    if row
        .get("auth_broken")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; login expired");
    }
    if row.get("keyless").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str("; no api key");
    }
    if row
        .get("canceled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; subscription canceled");
    }
    if let Some(rows) = row.get("throughput").and_then(Value::as_array)
        && !rows.is_empty()
    {
        out.push_str("; throughput: ");
        out.push_str(&throughput_prose(rows));
    }
    out
}

/// Prose for `profiles`. The all-scope roster is one `profile_line` per
/// profile; the session-scope arm is the folded-in former `which`: the one row
/// THIS session resolves to, rendered through the same `profile_line` (so it
/// inherits the roster's own guards), then how it resolved, then live usage and
/// the digest.
pub(crate) fn list_profiles_prose(p: &Value) -> String {
    if p.get("ok").and_then(Value::as_bool) == Some(false) {
        return format!(
            "error: {}",
            p.get("reason").and_then(Value::as_str).unwrap_or("unknown")
        );
    }
    let Some(rows) = p.get("profiles").and_then(Value::as_array) else {
        return "unknown".to_string();
    };
    if p.get("scope").and_then(Value::as_str) == Some("session") {
        // One row at most, with how it resolved, then the folded live usage
        // and digest (the roster arm carries neither).
        let row = rows.first();
        let (mut out, source) = match row {
            Some(row) => {
                let line = profile_line(row);
                let source = row.get("source").and_then(Value::as_str);
                (line, source)
            }
            // No row: `resolve_active` found nothing, which is an unresolved
            // session rather than an empty roster.
            None => ("session profile unknown, source unknown".to_string(), None),
        };
        // The live-usage fold names the CONFIGURED active profile, which this
        // scope's row need not be. When they are the same account the clause
        // restates the row's own headroom word for word, and the row already
        // marks it `(active)`, so the second copy is dropped rather than
        // rendered twice on one line. What must not drop with it is the age:
        // the row's figures are the ones the clause would date, and the row
        // renders `stale` itself, so the age rides the row BEFORE the source
        // clause, whose text it would otherwise read as dating.
        let lu = p.get("live_usage");
        let same_account = matches!(
            (
                row.and_then(|r| r.get("name")).and_then(Value::as_str),
                lu.and_then(|lu| lu.get("profile")).and_then(Value::as_str),
            ),
            (Some(row_name), Some(active)) if row_name == active
        );
        if same_account && let Some(lu) = lu {
            out.push_str(&age_clause(lu));
        }
        if let Some(source) = source {
            out.push_str(&format!("; source `{source}`"));
        }
        if let Some(lu) = lu.filter(|_| !same_account) {
            out.push_str("; ");
            out.push_str(&live_usage_prose(lu, "active profile"));
        }
        let digest = digest_prose(&p["since_your_last_call"]);
        if !digest.is_empty() {
            out.push_str("; ");
            out.push_str(&digest);
        }
        return out;
    }
    if rows.is_empty() {
        return "no profiles".to_string();
    }
    rows.iter().map(profile_line).collect::<Vec<_>>().join("\n")
}

/// Prose for `switch`: the outcome, then the active profile's live usage, then
/// the digest clause when the payload carries one.
pub(crate) fn switch_prose(p: &Value) -> String {
    let live = live_usage_prose(&p["live_usage"], "active profile");
    let digest = digest_prose(&p["since_your_last_call"]);
    let digest = if digest.is_empty() {
        String::new()
    } else {
        format!("; {digest}")
    };
    match p.get("ok").and_then(Value::as_bool) {
        Some(true) => {
            // A null `previous` is the logged-out state the switch started from
            // (clauth knows there was none), not a figure clauth lost.
            let previous = p
                .get("previous")
                .and_then(Value::as_str)
                .map_or_else(|| "none".to_string(), |v| format!("`{v}`"));
            let active = p
                .get("active")
                .and_then(Value::as_str)
                .map_or_else(|| "unknown".to_string(), |v| format!("`{v}`"));
            format!(
                "switched the global active profile from {previous} to {active}; {live}{digest}"
            )
        }
        _ => {
            let reason = p.get("reason").and_then(Value::as_str).unwrap_or("unknown");
            format!("switch failed: {reason}; {live}{digest}")
        }
    }
}

/// The token-usage object of a delegate envelope as one clause. The two fields
/// clauth's envelope contract documents read as English (`input N tokens`);
/// anything else claude put there (cache tiers arrive version-dependent) keeps
/// its field name in backticks so no figure silently vanishes.
fn usage_prose(u: &Value) -> String {
    let Some(obj) = u.as_object() else {
        return "unknown".to_string();
    };
    if obj.is_empty() {
        return String::new();
    }
    obj.iter()
        .map(|(k, v)| {
            let val = match v {
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::String(s) => s.clone(),
                Value::Null => "unknown".to_string(),
                other => other.to_string(),
            };
            let noun = match (k.as_str(), v) {
                ("input_tokens", Value::Number(_)) => Some("input"),
                ("output_tokens", Value::Number(_)) => Some("output"),
                _ => None,
            };
            match noun {
                Some(n) => format!("{n} {val} tokens"),
                None => format!("`{k}` {val}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Prose for a delegate envelope: the verdict (`finished` / `failed` / `timed
/// out`), the self-report, cost and tokens, then the kill/resume markers. The
/// raw envelope may carry more of claude's own fields; those stay in the JSON
/// spelling, and this names the fields clauth documents.
fn envelope_prose(e: &Value) -> String {
    let mut out = String::new();
    let ran_for = || {
        e.get("elapsed_secs")
            .and_then(Value::as_u64)
            .map_or_else(String::new, |el| format!(" after {el}s"))
    };
    // A cancel is read first and on its own key: it is a decision rather than a
    // deadline, so a cancelled envelope carries no `timed_out` for the arm below
    // to find, and "failed" would be the wrong word for a stop the caller asked
    // for.
    if e.get("cancelled").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str("cancelled");
        out.push_str(&ran_for());
    } else if let Some(t) = e.get("timed_out").and_then(Value::as_str) {
        out.push_str(&format!("timed out ({t})"));
        out.push_str(&ran_for());
    } else if e.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str("failed");
    } else {
        out.push_str("finished");
    }
    out.push_str(": ");
    // A bare scalar self-report (a non-object envelope the fold wrapped under
    // `result`) arrives as its own type; read it as its literal so a number or
    // bool never drops to `unknown`.
    out.push_str(&match e.get("result") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => "unknown".to_string(),
    });

    if let Some(cost) = e.get("total_cost_usd").and_then(Value::as_f64) {
        // `total_cost_usd` is the CHILD CLI's own figure, priced against
        // Anthropic's card whatever endpoint served the call, so a DeepSeek or
        // z.ai target's number is a wrong-basis figure a caller reads as the
        // bill. The endpoint arrives as data through the fold — this file
        // derives no figure the JSON did not carry.
        //
        // Three readings, kept apart for the same reason `live_usage_prose`
        // keeps its three: only a POSITIVE `anthropic` earns the bare clause;
        // a named other endpoint is known NOT to be Anthropic's; and an
        // unfolded envelope, or a target clauth could not classify, knows
        // neither — saying `not this endpoint's` there would assert an endpoint
        // nobody read.
        match e
            .get("live_usage")
            .and_then(|lu| lu.get("endpoint"))
            .and_then(Value::as_str)
        {
            Some("anthropic") => out.push_str(&format!(" (cost ${cost})")),
            Some(_) => out.push_str(&format!(
                " (cost ${cost} at Anthropic rates, not this endpoint's)"
            )),
            None => out.push_str(&format!(
                " (cost ${cost} at Anthropic rates, endpoint unknown)"
            )),
        }
    }
    if let Some(u) = e.get("usage") {
        let tokens = usage_prose(u);
        if !tokens.is_empty() {
            out.push_str(&format!(", usage: {tokens}"));
        }
    }
    if let Some(p) = e.get("partial_result").and_then(Value::as_str) {
        out.push_str(&format!("; partial result: {p}"));
    }
    if let Some(sid) = e.get("session_id").and_then(Value::as_str) {
        out.push_str(&format!("; resume with session id `{sid}`"));
    }
    if let Some(denials) = e.get("permission_denials")
        && !denials.is_null()
    {
        out.push_str(&format!("; permission denials: {denials}"));
    }
    out
}

/// Prose for `delegate`: the background handle or the sync envelope. Both carry
/// the folded live-usage footer and the digest — a handle is a reply about a
/// spend that just started, and the caller's next routing decision needs the
/// same headroom the blocking reply hands back.
pub(crate) fn delegate_prose(p: &Value) -> String {
    if let Some(job_id) = p.get("job_id").and_then(Value::as_str) {
        let profile = p
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let status = p.get("status").and_then(Value::as_str).unwrap_or("unknown");
        // A raw start epoch carries no news a reader acts on; the JSON spelling
        // keeps it. The handle's own spelling is unchanged: the bundled
        // `asyncRewake` hook scans this prose for `d-<ms>-<n>` tokens.
        let mut out = format!("delegate to `{profile}` {status}, job `{job_id}`");
        if let Some(lu) = p.get("live_usage") {
            out.push_str("; ");
            out.push_str(&live_usage_prose(lu, "target"));
        }
        let digest = digest_prose(&p["since_your_last_call"]);
        if !digest.is_empty() {
            out.push_str("; ");
            out.push_str(&digest);
        }
        return out;
    }
    let target = p
        .get("live_usage")
        .and_then(|lu| lu.get("profile"))
        .and_then(Value::as_str);
    let mut out = match target {
        Some(t) => format!("delegate to `{t}` {}", envelope_prose(p)),
        None => {
            let profile = p
                .get("profile")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("delegate to `{profile}` {}", envelope_prose(p))
        }
    };
    if let Some(lu) = p.get("live_usage") {
        out.push_str("; ");
        out.push_str(&live_usage_prose(lu, "target"));
    }
    let digest = digest_prose(&p["since_your_last_call"]);
    if !digest.is_empty() {
        out.push_str("; ");
        out.push_str(&digest);
    }
    out
}

/// Prose for a `delegate` argument/validation refusal. A refusal that fired
/// before target resolution carries the targets the caller spelled, so the
/// sentence names them; an envelope with no `profiles` (a refusal before any
/// target was named) reads plainly.
pub(crate) fn delegate_refusal_prose(p: &Value) -> String {
    let reason = p.get("result").and_then(Value::as_str).unwrap_or("unknown");
    match p.get("profiles").and_then(Value::as_array) {
        Some(names) => {
            let list = names
                .iter()
                .filter_map(Value::as_str)
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("delegate to {list} failed: {reason}")
        }
        None => format!("delegate failed: {reason}"),
    }
}

/// Prose for a `delegate` `profiles` fan-out: one job per named account, echoing
/// the resolved target list so the caller sees what it just spent, then each
/// target's own headroom.
///
/// The headroom clauses follow the id list rather than sitting inside each
/// parenthesis: the ids and the account names are what the caller (and the
/// `asyncRewake` hook) reads first, and a footer spliced between them would
/// bury the handles. The digest is the reply's, not a row's — it is folded once
/// at the top level, because reporting consumes the delta.
pub(crate) fn delegate_fanout_prose(p: &Value) -> String {
    let jobs = p
        .get("jobs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = String::from("delegated to ");
    for (i, job) in jobs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let profile = job
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let job_id = job
            .get("job_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        out.push_str(&format!("`{profile}` (job `{job_id}`)"));
    }
    for job in jobs {
        if let Some(lu) = job.get("live_usage") {
            out.push_str("; ");
            out.push_str(&live_usage_prose(lu, "target"));
        }
    }
    let digest = digest_prose(&p["since_your_last_call"]);
    if !digest.is_empty() {
        out.push_str("; ");
        out.push_str(&digest);
    }
    out
}

/// Prose for `monitor`'s one-id mode: the running status, the done envelope, or
/// an invalid/unknown job_id refusal.
pub(crate) fn delegate_result_prose(p: &Value) -> String {
    if p.get("job_id").and_then(Value::as_str).is_some()
        && p.get("status").and_then(Value::as_str).is_some()
    {
        return running_status_prose(p);
    }
    if let Some(lu) = p.get("live_usage") {
        let target = lu
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mut out = format!("delegate to `{target}` {}", envelope_prose(p));
        out.push_str("; ");
        out.push_str(&live_usage_prose(lu, "target"));
        let digest = digest_prose(&p["since_your_last_call"]);
        if !digest.is_empty() {
            out.push_str("; ");
            out.push_str(&digest);
        }
        return out;
    }
    let result = p.get("result").and_then(Value::as_str).unwrap_or("unknown");
    if p.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        format!("error: {result}")
    } else {
        result.to_string()
    }
}

/// The running-check line shared by `monitor`'s one-id status and each running
/// line of its several-ids reply, so the two spellings cannot drift: the job,
/// the account it spends, how long it has run, when it last said anything, how
/// far each deadline still is, that account's headroom, and — on its own
/// indented line — the newest thing the delegate wrote.
///
/// A run can be missing either deadline and still be perfectly healthy, so each
/// absence is NAMED rather than left to read as a lost figure: a streaming run
/// has no wall clock at all (the idle guard is its only deadline), and a run
/// whose caller pinned its own `--output-format` has no idle one (silence there
/// carries no information). Missing BOTH is the only case that means clauth is
/// short a fact rather than reporting one: every deadline is recorded together
/// at reserve time, so that job was started by a clauth which recorded neither.
pub(super) fn running_status_prose(p: &Value) -> String {
    let job_id = p.get("job_id").and_then(Value::as_str).unwrap_or("unknown");
    let status = p.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let elapsed = p
        .get("elapsed_secs")
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_string(), |v| format!("{v}s"));
    let mut out = format!("job `{job_id}` {status}");
    if let Some(profile) = p.get("profile").and_then(Value::as_str) {
        out.push_str(&format!(" on `{profile}`"));
    }
    out.push_str(&format!(", elapsed {elapsed}"));
    let wall = p.get("wall_kill_in_secs").and_then(Value::as_u64);
    let idle = p.get("idle_kill_in_secs").and_then(Value::as_u64);
    if wall.is_none() && idle.is_none() {
        out.push_str(", liveness not recorded (started under an older clauth)");
    } else {
        match p.get("last_output_secs_ago").and_then(Value::as_u64) {
            Some(secs) => out.push_str(&format!(", last output {secs}s ago")),
            None => out.push_str(", no output yet"),
        }
        match idle {
            Some(secs) => out.push_str(&format!(", idle-kill in {secs}s")),
            None => out.push_str(", no idle deadline"),
        }
        match wall {
            Some(secs) => out.push_str(&format!(", wall-kill in {secs}s")),
            None => out.push_str(", no wall clock"),
        }
    }
    if let Some(q) = p.get("quota") {
        out.push_str(&format!("; quota: {}", windows_prose(q)));
    }
    // Its own line, quoted: this is the delegate's words rather than clauth's
    // report about it. Escaped, because those words are ANOTHER account's model
    // output arriving verbatim in a model-facing reply, and a bare `"` in them
    // would close the span early and let the rest read as clauth's own prose.
    if let Some(tail) = p.get("tail").and_then(Value::as_str) {
        out.push_str(&format!("\n    \"{}\"", escape_quoted(tail)));
    }
    out
}

/// Escape a delegate's own text for the quoted span it lands in: backslashes
/// first, so an escape already in the text cannot consume the one added after
/// it, then the delimiter. `tail_line` has already collapsed every whitespace
/// run, so no newline can break the block shape either.
fn escape_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Prose for a `monitor` several-ids reply: one BLOCK per requested id, naming
/// its id and state, then the batch's own digest clause on a last line when it
/// carries one. A done line reuses the envelope spelling, a running line the
/// shared running spelling, an absent id reads `unknown`.
///
/// A block is usually one line but is not guaranteed to be: a done envelope's
/// `result` carries the delegate's own newlines, and a running job with a tail
/// puts that tail on its own indented line. What every block does guarantee is
/// that it OPENS with ``job `<id>` ``, which is what maps a wrapped line back to
/// the id that produced it.
///
/// The per-result live-usage fold stays out of the prose, so a batch of many
/// jobs does not repeat one account's percentages per line. The running blocks
/// do carry a quota clause, because a running check's whole job is to say
/// whether the account it is spending still has headroom.
pub(crate) fn delegate_result_batch_prose(p: &Value) -> String {
    let results = p
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = results
        .iter()
        .map(|r| {
            let job_id = r.get("job_id").and_then(Value::as_str).unwrap_or("unknown");
            match r.get("status").and_then(Value::as_str) {
                Some("done") => format!("job `{job_id}` {}", envelope_prose(r)),
                Some("running") => running_status_prose(r),
                _ => format!("job `{job_id}` unknown"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let digest = digest_prose(&p["since_your_last_call"]);
    if !digest.is_empty() {
        out.push('\n');
        out.push_str(&digest);
    }
    out
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_render.rs"]
mod tests;
