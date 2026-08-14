//! Pure formatters for the MCP layer: init instructions block, third-party
//! headline, and the prose spellings of each tool's JSON payload, the
//! folded-in live-usage clause included. No I/O, no locks — callers pass in
//! already-loaded cache data so these stay unit-testable.

use serde_json::Value;

use crate::format::format_pct;
use crate::providers::ThirdPartyStats;
use crate::which::SessionAuth;

/// Per-profile snapshot fed to [`instructions_block`]: stable identity only (name,
/// provider, tier, base url). Volatile usage figures rot within a turn, so they are
/// served fresh per call by `list_profiles`, never baked into the boot-time block.
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
/// endpoint path, so both the roster and `list_profiles` print the identifying
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

/// What a `switch` does to *this* session, keyed on how it reads its credentials.
/// A global session reads the exact file `switch` repoints; an isolated session
/// (a `clauth start` runtime or a custom `CLAUDE_CONFIG_DIR`) reads its own, so a
/// switch can't disturb it. Pure mapping — the caller resolves the [`SessionAuth`].
pub(crate) fn switch_effect(auth: &SessionAuth) -> String {
    match auth {
        SessionAuth::Global => "`switch` repoints the global `~/.claude` credentials THIS \
session reads; Claude Code reloads them on its next token refresh, so this session would \
start acting as the switched profile mid-task. To use another account \
without disturbing this one, use the `delegate` tool."
            .to_string(),
        SessionAuth::IsolatedRuntime(name) => format!(
            "`switch` repoints the global `~/.claude` credentials, but THIS session runs in an \
isolated `clauth start` runtime pinned to `{name}` and is unaffected. Only a later session on \
the global credentials adopts the change."
        ),
        SessionAuth::IsolatedCustom => "`switch` repoints the global `~/.claude` credentials, but \
THIS session uses a custom `CLAUDE_CONFIG_DIR` and reads its own credentials, so it is \
unaffected. Only a later session on the global credentials adopts the change."
            .to_string(),
    }
}

/// How this session's runtime tree maps onto the real global one, for the only
/// tier that has such a tree. A `clauth start` runtime looks per-profile and is
/// mostly symlinks onto `~/.claude/`, so a model editing `CLAUDE.md` or
/// `skills/…` under it is editing the global file. The note names
/// `$CLAUDE_CONFIG_DIR` rather than a constructed path: the real dir carries a
/// per-session suffix (`runtime-<sid>`, the sid being `<pid>-<seq>`), so any
/// literal spelled here would point at a directory that does not exist.
///
/// It also names no destination past `~/.claude/`. Whether an entry there chains
/// on somewhere else is the operator's own layout rather than anything clauth
/// builds: this box reaches `~/.agents/skills` through a `~/.claude/skills`
/// symlink the operator made, and a box without it would be told a falsehood.
/// The closing `readlink -f` covers the general case for every box.
///
/// `Global` has no runtime dir, and `IsolatedCustom` is a foreign
/// `CLAUDE_CONFIG_DIR` whose layout clauth does not own — neither may claim this
/// layout. Pure mapping; the caller resolves the [`SessionAuth`].
pub(crate) fn runtime_paths_note(auth: &SessionAuth) -> Option<String> {
    match auth {
        SessionAuth::IsolatedRuntime(name) => Some(format!(
            "runtime paths: this session's config dir (`$CLAUDE_CONFIG_DIR`, profile `{name}`) \
is mostly SYMLINKS onto the global `~/.claude/<same-name>`. Only `.claude.json`, `settings.json` \
and `.credentials.json` are per-profile. So a write under that dir lands in the global file every \
profile and every future session loads, and a rule gating `~/.claude/` binds through it too. \
`readlink -f` before treating a path as profile-local."
        )),
        SessionAuth::Global | SessionAuth::IsolatedCustom => None,
    }
}

/// Init-time `instructions` block: identity intro, a one-line tool router, a
/// session-aware `switch` note, the runtime-path note that tier earns, the
/// `delegate` cost model, then the grouped roster. This block is the only clauth
/// text a session is guaranteed to hold: tool descriptions are deferred in some
/// harnesses and unloaded until searched for, so the router line stays even
/// though every tool carries its own description. Per-tool mechanics do NOT stay
/// — they live in that tool's description, which is loaded by the time anyone
/// can call it. No usage percentage or reset timer is baked in; those rot within
/// a turn, so they live in `list_profiles`.
pub(crate) fn instructions_block(profiles: &[ProfileSnapshot], auth: &SessionAuth) -> String {
    let mut out = String::new();
    out.push_str(
        "clauth manages multiple Claude Code accounts (\"profiles\"): each an isolated \
credential set / subscription. Use its tools to compare usage headroom across accounts, relink \
the active account, or delegate a task to another account without spending this session's \
window.\n\n\
Tools: `list_profiles` (roster + cached usage, zero quota), `which` (this session's own profile), \
`switch` (relink the global active profile), `delegate` (run a task on another account), \
`delegate_result` (collect a backgrounded delegate).\n\n\
switch & this session: ",
    );
    out.push_str(&switch_effect(auth));
    if let Some(note) = runtime_paths_note(auth) {
        out.push_str("\n\n");
        out.push_str(&note);
    }
    out.push_str(
        "\n\nCost: a `delegate` to a profile with no endpoint host burns that subscription's \
rate-limited window; to DeepSeek or Z.ai it bills real money; to Alibaba Model Studio it draws \
down a prepaid plan quota; to a loopback or LAN host it is free. Call `list_profiles` for live \
windows and third-party balances.\n\n\
Profiles, most headroom first (session-start snapshot; call `list_profiles` for live usage and \
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
/// for the profile it names: `active profile` for `which`/`switch`, `target` for
/// `delegate`. A null profile name reads `none` (no active profile is
/// configured — a state clauth knows, not a missing figure); a null window
/// reads `unknown`.
pub(crate) fn live_usage_prose(lu: &Value, lead: &str) -> String {
    let name = lu
        .get("profile")
        .and_then(Value::as_str)
        .map_or_else(|| "none".to_string(), |n| format!("`{n}`"));
    let five = lu.get("5h_used_pct").and_then(Value::as_f64);
    let seven = lu.get("7d_used_pct").and_then(Value::as_f64);
    let mut out = format!(
        "{lead} {name}: 5h {}, 7d {}",
        pct_clause(five),
        pct_clause(seven)
    );
    if let Some(w) = lu.get("throughput_warning").and_then(Value::as_str) {
        out.push_str("; ");
        out.push_str(w);
    }
    out
}

/// The `windows` array (or `quota` array) as one clause. Empty array is `usage
/// unknown`: no cache is not a zero.
fn windows_prose(windows: &Value) -> String {
    let Some(ws) = windows.as_array() else {
        return "usage unknown".to_string();
    };
    if ws.is_empty() {
        return "usage unknown".to_string();
    }
    ws.iter()
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
        .join(", ")
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
/// `[provider, tier, host]` bracket, the live windows, the third-party
/// headline, then the quiet flags. A null tier on an anthropic account and a
/// missing third-party balance both read `unknown` rather than dropping out.
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

    // `third_party` is null for every anthropic profile (not third-party at all)
    // AND for a third-party profile with no cache yet; `provider` disambiguates.
    let third = row.get("third_party").and_then(Value::as_str);
    match (provider, third) {
        (_, Some(t)) if !t.is_empty() => {
            out.push_str(&format!("; {t}"));
        }
        ("anthropic", _) => {}
        (_, _) => out.push_str("; balance unknown"),
    }

    // A null tier is structural for third-party accounts, but on an anthropic
    // account it means the plan is unknown.
    if tier.is_none() && provider == "anthropic" {
        out.push_str("; tier unknown");
    }
    if row
        .get("has_live_session")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; live session");
    }
    if let Some(rows) = row.get("throughput").and_then(Value::as_array)
        && !rows.is_empty()
    {
        out.push_str("; throughput: ");
        out.push_str(&throughput_prose(rows));
    }
    out
}

/// Prose for `list_profiles`: its error envelope, or one line per profile.
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
    if rows.is_empty() {
        return "no profiles".to_string();
    }
    rows.iter().map(profile_line).collect::<Vec<_>>().join("\n")
}

/// Prose for `which`: session identity, throughput when observed, then the
/// active profile's live usage.
pub(crate) fn which_prose(p: &Value) -> String {
    let profile = p
        .get("profile")
        .and_then(Value::as_str)
        .map_or_else(|| "unknown".to_string(), |v| format!("`{v}`"));
    let source = p
        .get("source")
        .and_then(Value::as_str)
        .map_or_else(|| "unknown".to_string(), |v| format!("`{v}`"));
    let tier = p
        .get("tier")
        .and_then(Value::as_str)
        .map_or_else(|| "unknown".to_string(), |v| format!("`{v}`"));
    let mut out = format!("session profile {profile}, source {source}, tier {tier}");
    if let Some(rows) = p.get("throughput").and_then(Value::as_array)
        && !rows.is_empty()
    {
        out.push_str("; throughput: ");
        out.push_str(&throughput_prose(rows));
    }
    out.push_str("; ");
    out.push_str(&live_usage_prose(&p["live_usage"], "active profile"));
    out
}

/// Prose for `switch`: the outcome, then the active profile's live usage.
pub(crate) fn switch_prose(p: &Value) -> String {
    let live = live_usage_prose(&p["live_usage"], "active profile");
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
            format!("switched the global active profile from {previous} to {active}; {live}")
        }
        _ => {
            let reason = p.get("reason").and_then(Value::as_str).unwrap_or("unknown");
            format!("switch failed: {reason}; {live}")
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
    if let Some(t) = e.get("timed_out").and_then(Value::as_str) {
        out.push_str(&format!("timed out ({t})"));
        if let Some(el) = e.get("elapsed_secs").and_then(Value::as_u64) {
            out.push_str(&format!(" after {el}s"));
        }
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
        out.push_str(&format!(" (cost ${cost})"));
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

/// Prose for `delegate`: the background handle or the sync envelope.
pub(crate) fn delegate_prose(p: &Value) -> String {
    if let Some(job_id) = p.get("job_id").and_then(Value::as_str) {
        let profile = p
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let status = p.get("status").and_then(Value::as_str).unwrap_or("unknown");
        // A raw start epoch carries no news a reader acts on; the JSON spelling
        // keeps it.
        return format!("delegate to `{profile}` {status}, job `{job_id}`");
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
    out
}

/// Prose for a `delegate` argument/validation refusal. A refusal that fired
/// before target resolution carries the target the caller spelled, so the
/// sentence names it; an envelope with neither `profile` nor `profiles` (a
/// refusal before any target was named) reads plainly.
pub(crate) fn delegate_refusal_prose(p: &Value) -> String {
    let reason = p.get("result").and_then(Value::as_str).unwrap_or("unknown");
    match (
        p.get("profile").and_then(Value::as_str),
        p.get("profiles").and_then(Value::as_array),
    ) {
        (Some(t), _) => format!("delegate to `{t}` failed: {reason}"),
        (None, Some(names)) => {
            let list = names
                .iter()
                .filter_map(Value::as_str)
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("delegate to {list} failed: {reason}")
        }
        (None, None) => format!("delegate failed: {reason}"),
    }
}

/// Prose for a `delegate` `profiles` fan-out: one job per named account, echoing
/// the resolved target list so the caller sees what it just spent.
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
    out
}

/// Prose for `delegate_result`: the running status (with optional `quota`), the
/// done envelope, or an invalid/unknown job_id refusal.
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
        return out;
    }
    let result = p.get("result").and_then(Value::as_str).unwrap_or("unknown");
    if p.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        format!("error: {result}")
    } else {
        result.to_string()
    }
}

/// The `job_id` + `status` line shared by the single `delegate_result` running
/// status and each running line of a batch.
fn running_status_prose(p: &Value) -> String {
    let job_id = p.get("job_id").and_then(Value::as_str).unwrap_or("unknown");
    let status = p.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let elapsed = p
        .get("elapsed_secs")
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_string(), |v| format!("{v}s"));
    let mut out = format!("job `{job_id}` {status}, elapsed {elapsed}");
    if let Some(q) = p.get("quota") {
        out.push_str(&format!("; quota: {}", windows_prose(q)));
    }
    out
}

/// Prose for a `delegate_result` batch: one line per requested id, naming its
/// id and state. A done line reuses the envelope spelling, a running line the
/// shared running spelling, an absent id reads `unknown`. Live usage stays in
/// the JSON spelling only so the lines stay short.
pub(crate) fn delegate_result_batch_prose(p: &Value) -> String {
    let results = p
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    results
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
        .join("\n")
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_render.rs"]
mod tests;
