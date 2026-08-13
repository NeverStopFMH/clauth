//! Pure formatters for the MCP layer: init instructions block, per-call live
//! footer, single usage line, third-party headline. No I/O, no locks — callers
//! pass in already-loaded cache data so these stay unit-testable.

use crate::format::format_pct;
use crate::providers::ThirdPartyStats;
use crate::usage::UsageWindow;
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
    /// Percent of this profile's best-known window still FREE, for roster
    /// ordering only. `None` for a provider that reports a balance instead of a
    /// window: ranking those would mean comparing a USD figure against a CNY
    /// one, so they keep config order at the end.
    pub(crate) headroom_pct: Option<f64>,
}

/// Host (and port) of a base url. Every profile of one provider carries the same
/// endpoint path, so the roster prints the identifying half only.
fn base_url_host(url: &str) -> &str {
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

/// Unknown headroom ranks below a fully-spent window, so a profile clauth has no
/// figure for never outranks one it knows is free.
fn rank(p: &ProfileSnapshot) -> f64 {
    p.headroom_pct.unwrap_or(-1.0)
}

/// Roster body: one line per distinct bracket, names joined, most headroom first.
/// A fleet of same-provider profiles otherwise repeats one identical endpoint on
/// every line, which is pure token cost in a block every session loads. The
/// ordering is a hint rather than a claim — it freezes at server start like the
/// rest of the roster, which is why the header calls it a snapshot.
fn roster_lines(profiles: &[ProfileSnapshot]) -> String {
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
    fn best(members: &[&ProfileSnapshot]) -> f64 {
        members.first().map_or(-1.0, |p| rank(p))
    }
    for (_, members) in &mut groups {
        members.sort_by(|a, b| rank(b).total_cmp(&rank(a)));
    }
    groups.sort_by(|a, b| best(&b.1).total_cmp(&best(&a.1)));

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

/// Compact freshness footer appended to every `which`/`switch`/`delegate` result:
/// active profile + 5h/7d percent-used for the touched profile, read fresh from
/// cache. Percentages are the share of the window consumed (higher = less
/// headroom), labeled `% used` so the reader can't invert it.
pub(crate) fn live_footer(
    active: Option<&str>,
    five_h: Option<&UsageWindow>,
    seven_d: Option<&UsageWindow>,
) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(a) = active {
        parts.push(format!("active={a}"));
    }
    if let Some(w) = five_h {
        parts.push(format!("5h {} used", format_pct(w.utilization)));
    }
    if let Some(w) = seven_d {
        parts.push(format!("7d {} used", format_pct(w.utilization)));
    }
    parts.join(" | ")
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
/// per-session suffix (`runtime-<sid>-<n>`), so any literal spelled here would
/// point at a directory that does not exist. `Global` has no runtime dir,
/// and `IsolatedCustom` is a foreign `CLAUDE_CONFIG_DIR` whose layout clauth does
/// not own — neither may claim this layout. Pure mapping; the caller resolves the
/// [`SessionAuth`].
pub(crate) fn runtime_paths_note(auth: &SessionAuth) -> Option<String> {
    match auth {
        SessionAuth::IsolatedRuntime(name) => Some(format!(
            "runtime paths: this session's config dir (`$CLAUDE_CONFIG_DIR`, profile `{name}`) \
is mostly SYMLINKS onto the global `~/.claude/<same-name>`, and its `skills` chains on to \
`~/.agents/skills`. Only `.claude.json`, `settings.json` and `.credentials.json` are per-profile. \
So a write under that dir lands in the global file every profile and every future session loads, \
and a rule gating `~/.claude/` or `~/.agents/` binds through it too. `readlink -f` before \
treating a path as profile-local."
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

#[cfg(test)]
#[path = "../../tests/inline/mcp_render.rs"]
mod tests;
