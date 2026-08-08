//! Pure profile/usage → display-string formatters, plus the cross-surface
//! diagnostic messages. No UI dependencies, so the TUI, the CLI subcommands
//! (e.g. `clauth which`), and the headless daemon all share one spelling.
//!
//! Deliberately the ratatui-free tier only. Helpers that emit `Span`/`Style`
//! live in `tui/render/format.rs`; single-screen or domain-local display glue
//! stays with its owner. Folding those in would force ratatui into this shared
//! module or mint one-caller abstractions, so the split stands over a single
//! grab-bag `format.rs` (surveyed 2026-07-16).

use crate::profile::Profile;
use crate::usage::PlanTier;

// ── Cross-surface diagnostics ───────────────────────────────────────────────
//
// A condition that surfaces on more than one surface — a CLI `bail!`, a daemon
// `logline!`, a TUI toast — is worded here once. Each surface used to spell the
// same event its own way and they drifted (one condition printed four different
// sentences). `head` is the at-a-glance summary; `detail` the cause and the
// recovery step.

/// One diagnostic, rendered per surface. Keep `head` short enough to read on a
/// toast's bold first line without wrapping; put the cause and next step in
/// `detail`.
pub(crate) struct Message {
    head: String,
    detail: Option<String>,
}

impl Message {
    /// Single-line form for a CLI `bail!` or a `logline!` body (`head: detail`).
    /// The caller prepends any `clauth `/`clauth daemon: ` log prefix.
    pub(crate) fn line(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}: {}", self.head, d),
            None => self.head.clone(),
        }
    }

    /// Toast form: `head` on its own line, `detail` below it. The toast renderer
    /// styles line 1 bold and the rest dim, so the split reads as summary + note.
    pub(crate) fn toast(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}\n{}", self.head, d),
            None => self.head.clone(),
        }
    }

    /// The next step alone, for a surface whose own first line ALREADY states
    /// the condition — the rotate toast opens with `refresh for 'X' failed`, so
    /// rendering a whole `Message` under it named the account three times.
    /// Falls back to `head` when there is no detail, so a caller never has to
    /// mint copy of its own; minting is the drift this module exists to stop.
    pub(crate) fn detail(&self) -> &str {
        self.detail.as_deref().unwrap_or(&self.head)
    }
}

/// What to tell the operator to do about a transient failure.
///
/// Travels INSIDE [`Transient`] rather than arriving as a parameter: three
/// surfaces render [`refresh_transient`], and a `kind` argument would re-scatter
/// this choice across exactly the call sites this module exists to unify.
pub(crate) enum Retry {
    /// A transport failure — the connection is the thing worth checking.
    Connection,
    /// Upstream is throttling, busy, or briefly broken. Waiting is the fix, and
    /// telling someone to check their connection over a 429 is wrong advice.
    Wait,
    /// The cause already names its own next step, so a second one would
    /// contradict it (`check permissions on ~/.clauth` followed by `check your
    /// connection and retry` gives two different and incompatible reasons to
    /// retry, one of which is wrong).
    Stated,
    /// There is nothing left to retry in-process: `login_with` has no retry
    /// path around its code exchange, so whatever the status, the only action
    /// available is running `clauth login` again. Stated as the ABSENCE of a
    /// retry loop rather than as a fact about the code or the listener, because
    /// this correctly stops being true the moment someone adds one.
    Restart,
}

/// Every transient cause clauth can state, as a CLOSED set.
///
/// Deliberately not one open `String` field. The historically-real accident is
/// `format!("{status}: {body}")` handed to a free-text cause, and that no longer
/// has an arm to land in: the caller must pick one that describes what actually
/// happened, which is a question a response body does not answer.
///
/// What the types ENFORCE is narrower than that, and worth stating exactly.
/// Only [`Self::Endpoint`] is sealed — `&'static str` cannot hold a
/// runtime-allocated response body, and it takes precisely what
/// `TokenFailure::user_message` returns. [`Self::RotationLockUnavailable`] and
/// [`Self::PersistFailed`] still hold a `String`; what keeps THOSE honest is
/// that each renders its own fixed sentence below and interpolates the value as
/// a profile name, so a body passed there would read as an account name and
/// nothing else. Sealing all four means a newtype only the callers can mint;
/// worth doing if a fifth arm ever needs a runtime value that is not a name.
pub(crate) enum Cause {
    /// Already-canned copy from `oauth::TokenFailure`.
    Endpoint(&'static str),
    /// The per-profile rotation lock could not be CREATED or OPENED — a
    /// filesystem or permissions problem under `~/.clauth`.
    ///
    /// Not contention, despite what this arm used to say. `RotationGuard::
    /// acquire` ends in a blocking `File::lock()`, so a sibling worker or a live
    /// session holding the lock makes the caller WAIT; it can never surface
    /// here. The old copy told the operator to wait for an in-flight refresh
    /// that does not exist, and a test pinned that wording.
    RotationLockUnavailable(String),
    /// A poisoned mutex: another thread panicked, so it will not clear itself
    /// and a retry hint would be a lie.
    InternalLock,
    /// The refresh landed but the rotated pair could not be written.
    PersistFailed(String),
    /// CLA-ROLL: the rolling session token could not be written to (or restored
    /// into) the profile's sidecar. The chain itself is fine — what failed is
    /// the file in front of it, so this is a filesystem problem and not an
    /// account one.
    SidecarWriteFailed(String),
    /// CLA-ROLL: a live `clauth start` session is holding this profile's
    /// ROTATING pair, because it started before the sidecar was armed. Spending
    /// the refresh now revokes the chain under a running session, which is the
    /// exact death the static-token split exists to prevent.
    ///
    /// Distinct from [`Self::RotationLockUnavailable`] on purpose: nothing is
    /// locked and nothing is broken. The next step is the operator's, and it is
    /// specific enough that a generic retry hint would be wrong.
    LiveSessionOnRotatingChain(String),
    /// CLA-ROLL: another holder has the profile's rotation lock and the caller
    /// runs on a thread that must not park behind it (the scheduler's re-stamp
    /// leg). Genuine contention — the opposite claim from
    /// [`Self::RotationLockUnavailable`], which is why it is not that arm: the
    /// holder's own path usually re-stamps the sidecar itself, and the scan
    /// retries in minutes against an hours-wide horizon either way.
    RotationLockHeld(String),
    /// CLA-ROLL: the usage chain's RECORDED grant cannot be told from a
    /// setup-token mint (no scope beyond the setup pair, no plan stamp), so
    /// stamping a rolling bearer from it is refused — the bearer could later
    /// be preserved as "the mint". Not a filesystem problem and not retryable
    /// in-process: only a fresh `clauth login` records the chain's real grant.
    RollingGrantUnrecorded(String),
    /// CLA-ROLL: the sidecar holds a rotating pair (mis-filled) with no live
    /// mint backup to heal it, and the caller runs on the thread that must not
    /// fall into the blocking vanilla gate (the scheduler's re-stamp leg —
    /// which also has no re-stamp work to do on a disengaged split). Not
    /// retryable in-process: only a fresh `clauth login <p> --setup-token`
    /// re-captures the mint.
    SidecarMisfilled(String),
}

impl Cause {
    fn text(&self) -> String {
        match self {
            Self::Endpoint(canned) => (*canned).to_string(),
            Self::RotationLockUnavailable(profile) => {
                format!(
                    "could not lock '{profile}' for a token refresh; check permissions on ~/.clauth"
                )
            }
            Self::InternalLock => "clauth hit an internal lock error, restart clauth".to_string(),
            Self::SidecarWriteFailed(profile) => {
                format!(
                    "could not write '{profile}' session token · check permissions on ~/.clauth"
                )
            }
            Self::LiveSessionOnRotatingChain(profile) => {
                format!(
                    "'{profile}' has a live clauth start session holding its rotating chain \
                     (it started before the rolling token was armed); restart that session or \
                     retry once it ends"
                )
            }
            Self::RotationLockHeld(profile) => {
                format!(
                    "an in-flight rotation holds '{profile}' · the re-stamp retries on its \
                     next scan"
                )
            }
            Self::RollingGrantUnrecorded(profile) => {
                format!(
                    "'{profile}' usage chain has no recorded grant beyond the setup-token \
                     scopes, so a rolling bearer cannot be told from a mint · run \
                     `clauth login {profile}` to record the chain's real grant"
                )
            }
            Self::SidecarMisfilled(profile) => {
                format!(
                    "'{profile}' session token holds a rotating pair and no live mint backup \
                     exists to heal it · re-capture with `clauth login {profile} --setup-token`"
                )
            }
            Self::PersistFailed(profile) => {
                format!("refreshed '{profile}' but failed to persist the rotated tokens")
            }
        }
    }
}

/// A transient failure carrying its own next step, and the HTTP status when the
/// failure had one.
///
/// The status is deliberately separable: CLI stderr and the daemon log show it
/// (neither has a companion log to read it out of) while a toast and the MCP
/// payload do not.
pub(crate) struct Transient {
    cause: Cause,
    status: Option<u16>,
    retry: Retry,
}

impl Transient {
    pub(crate) fn new(cause: Cause, retry: Retry) -> Self {
        Self {
            cause,
            status: None,
            retry,
        }
    }

    pub(crate) fn with_status(cause: Cause, status: u16, retry: Retry) -> Self {
        Self {
            cause,
            status: Some(status),
            retry,
        }
    }

    fn suffix(&self) -> &'static str {
        match self.retry {
            Retry::Connection => ": check your connection and retry",
            Retry::Wait => ": retry in a moment",
            Retry::Stated => "",
            Retry::Restart => ": run clauth login again for a fresh code",
        }
    }

    /// Cause + next step, no status. TUI toasts and the MCP `reason`.
    pub(crate) fn text(&self) -> String {
        format!("{}{}", self.cause.text(), self.suffix())
    }

    /// The causes only a fresh `clauth login` clears — no in-process retry
    /// can: an unrecorded chain grant ([`Cause::RollingGrantUnrecorded`]) and
    /// a mis-filled sidecar with nothing live to heal it
    /// ([`Cause::SidecarMisfilled`]). The scheduler paces these on the same
    /// long leash as a `Broken` verdict — a minutes-scale retry against a
    /// condition no retry can clear is pure log noise. The leash never delays
    /// the recovery it prescribes: the re-login that clears these re-arms the
    /// rolling token and stamps the sidecar itself, CLI-side, without waiting
    /// for any scan (the same property the `Broken` leash already leans on).
    pub(crate) fn permanent_until_relogin(&self) -> bool {
        matches!(
            self.cause,
            Cause::RollingGrantUnrecorded(_) | Cause::SidecarMisfilled(_)
        )
    }

    /// Cause + status + next step. CLI stderr and the daemon log, the two
    /// surfaces with no companion log to read the status out of.
    pub(crate) fn text_with_status(&self) -> String {
        match self.status {
            Some(s) => format!("{} (HTTP {s}){}", self.cause.text(), self.suffix()),
            None => self.text(),
        }
    }
}

/// A login whose refresh token is dead: re-login is the only fix. Shared by the
/// CLI/MCP switch bail, the daemon tick log, and the TUI switch toast.
pub(crate) fn login_expired(name: &str) -> Message {
    Message {
        head: format!("login for '{name}' has expired"),
        detail: Some(format!(
            "refresh token revoked or invalid: run clauth login {name}"
        )),
    }
}

/// A refresh that failed for a transient reason: this switch is refused but the
/// login is not quarantined. The next step comes from `err`'s own [`Retry`], so
/// a throttle is never told to check its connection.
pub(crate) fn refresh_transient(name: &str, err: &Transient) -> Message {
    Message {
        head: format!("could not refresh '{name}' before switching"),
        detail: Some(err.text()),
    }
}

/// [`refresh_transient`] for CLI stderr, which additionally names the HTTP
/// status. Split as a second constructor rather than a flag, because `line()`
/// serves BOTH the CLI bail and the MCP payload — the surface split cannot be
/// made on the renderer.
pub(crate) fn refresh_transient_cli(name: &str, err: &Transient) -> Message {
    Message {
        head: format!("could not refresh '{name}' before switching"),
        detail: Some(err.text_with_status()),
    }
}

/// The one spelling for "go fix this in the app". The surface is the `clauth`
/// TUI, never a bare "the TUI" (which reads as some other UI).
pub(crate) const RESOLVE_IN_TUI: &str = "resolve the divergence in the clauth TUI";

/// The `s` a count needs, per cloudy-tui's counts rule: singular at one.
pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Trailing-ellipsis truncation to `max` chars (counts `char`s, not bytes).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The account's tier: a fetched plan wins when classified, else the OAuth
/// token's `subscription_type` claim, else `None` when neither names one. A
/// surface renders its own no-data form on `None` rather than a bare "Claude"
/// that reads as a real plan.
pub(crate) fn account_tier(profile: &Profile) -> Option<PlanTier> {
    // A fetched tier wins, but an UNCLASSIFIED one is not an answer: fall through
    // the way `profile_json::tier_label` does, or this surface reads "no data"
    // while that one shows the token's tier for the very same account.
    let fetched = profile
        .usage
        .as_ref()
        .and_then(|u| u.plan.as_ref())
        .map(|p| p.tier.clone())
        .filter(|t| *t != PlanTier::Unknown);
    fetched.or_else(|| {
        // No fetched plan yet — fall back to the OAuth token's subscription_type.
        let sub = profile
            .credentials
            .as_ref()
            .and_then(|c| c.claude_ai_oauth.as_ref())
            .and_then(|o| o.subscription_type.as_deref());
        match PlanTier::from_subscription_type(sub) {
            PlanTier::Unknown => None,
            tier => Some(tier),
        }
    })
}

/// Percent from API `f64`: drops trailing `.0` on whole numbers → `42%`, `42.3%`.
pub(crate) fn format_pct(pct: f64) -> String {
    if pct.fract() == 0.0 {
        format!("{pct:.0}%")
    } else {
        format!("{pct}%")
    }
}

#[cfg(test)]
#[path = "../tests/inline/format.rs"]
mod tests;
