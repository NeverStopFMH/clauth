//! `clauth hook-profile-changed-note` — tell a running conversation when the
//! account behind it changed.
//!
//! A conversation can move accounts three ways and none of them says so: a
//! resume under another profile keeps the Claude Code session id and appends to
//! the same transcript, a `clauth switch` lands while a global session works,
//! and a `--with-fallback` session executes a credential swap mid-run. One
//! predicate answers all three — which account this conversation's credentials
//! resolve to right now, against the last value clauth told this conversation —
//! so this reads a hook payload on stdin and emits `additionalContext` only when
//! those two differ.
//!
//! **Not the MCP `instructions` block.** That block is built once per process,
//! so it cannot carry a mid-conversation change at all, and rewriting the front
//! of a live context invalidates the cached prefix behind it.
//!
//! Three properties carry the design:
//!
//! - **The account comes from the tier walk, never the runtime directory name.**
//!   After a swap the directory keeps the profile the session LAUNCHED on while
//!   the credential link points elsewhere, so a path-derived name answers "which
//!   directory" rather than "which account".
//! - **A stat gates the resolution, and a TTL bounds what the stat misses.**
//!   This runs on every tool call, so the record carries a stamp of the
//!   resolution's inputs and skips it when nothing moved. The stamp is the
//!   credential store plus a hash of [`crate::profile::reload_fingerprint`],
//!   which is the crate's own predicate for "could a config reload change the
//!   answer" and covers every per-profile `config.toml` that two hand-rolled
//!   stats did not. [`RESOLUTION_TTL`] is the correctness backstop rather than
//!   an optimisation: it turns anything the fingerprint still misses from an
//!   unbounded miss into a bounded one. Two costs, and the doc used to price
//!   only the first (both measured by review, 2026-08-21, debug build):
//!   a fire that OPENS the gate runs `load_config`, which chmod-walks the whole
//!   `~/.clauth` tree, so this process mutates the filesystem when it resolves
//!   (~3.1 ms at 0 entries, ~5.9 ms at 2000, against a ~2.2 ms spawn floor);
//!   and `reload_fingerprint` runs on EVERY fire, open or closed, at a readdir
//!   plus two stats per profile (+342 µs at 2 profiles, +523 at 30, +651 at 60).
//!   The second is the price of the gate being sound at all, and it scales with
//!   profile count rather than with anything this module controls.
//! - **One record per SCOPE, not per conversation.** A `PostToolUse` fired
//!   inside a subagent carries `agent_id`; a single shared told-flag would let
//!   the first subagent to fire consume the note while the main thread never
//!   hears it. Separate files keep two SCOPES off each other's bytes, and that
//!   is all they do: every main-thread parallel tool call fires with no
//!   `agent_id` and so shares one record, which is why the read-modify-write
//!   below is flock-held (measured by review: 4 concurrent fires emitted the
//!   note 2-4 times in 30 of 30 trials before the lock).
//!
//! A failure is silence at exit 0 wherever this module can make it one, because
//! a hook that errors on a tool call breaks the conversation it exists to
//! inform. The one path it does not own is [`crate::out`]: a stdout write error
//! that is not `BrokenPipe` panics there by that module's deliberate contract,
//! which exits 101.

use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::out::outln;
use crate::profile::atomic_write_600;

/// Dir under `~/.clauth` holding one record per conversation scope.
const RECORDS_DIR: &str = "conversations";

/// How long a record whose transcript is not on disk survives the sweep.
///
/// The grace belongs on THIS branch, not on the ageing one below: a baseline
/// recorded at `SessionStart` can land before Claude Code has created the
/// transcript file, and a bare `!exists()` then lets any `clauth` invocation on
/// the box reap a live conversation's record — after which its next real account
/// move is absorbed as a fresh baseline and never announced.
///
/// It is measured from the record's MTIME, so the quantity it bounds is time
/// since this scope last WROTE, not time since the transcript went missing. A
/// conversation still opening the gate keeps rewriting the record and so never
/// elapses it, which is the intent; the two only coincide for a scope that has
/// gone quiet.
const MISSING_TRANSCRIPT_GRACE: Duration = Duration::from_secs(60 * 60);

/// How long a record that never carried a `transcript_path` survives. Nothing
/// can test such a record for liveness, so it ages out instead.
const ORPHAN_RECORD_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a resolution may be reused before it is retaken regardless of the
/// stamp. The stamp is an optimisation and this is the correctness bound: any
/// input `reload_fingerprint` does not cover (a per-profile `credentials.json`
/// write that never touches the live link) costs at most this much staleness
/// rather than an unbounded miss.
const RESOLUTION_TTL: Duration = Duration::from_secs(60);

/// Longest stdin this will read. `PostToolUse` embeds a whole `tool_response`,
/// and the hook manifest's `timeout` bounds TIME rather than memory: reading an
/// unbounded stream reached 28.4 GB RSS in review. Matches the 10 MB cap
/// `update.rs` already puts on a downloaded asset.
const MAX_PAYLOAD_BYTES: u64 = 10 * 1024 * 1024;

/// The two spellings, behind one renderer so they cannot drift apart.
///
/// The noun is "session", by owner ruling on 2026-08-21, superseding an earlier
/// one here that said "conversation" and never "session". Carry the cost that
/// ruling turned on rather than deleting it: every other "session" in
/// model-facing clauth copy names the PROCESS, so after a swap the MCP block's
/// runtime-paths note and this note both say "session" about two things that
/// disagree. Do not resolve that by mutating the block, which is settled against.
enum Note<'a> {
    /// A new Claude Code process picked this conversation up on another account.
    Resumed { now: &'a str, before: &'a str },
    /// The account moved under a conversation that was already running.
    Switched { from: &'a str, to: &'a str },
}

impl Note<'_> {
    fn render(&self) -> String {
        match *self {
            Note::Resumed { now, before } => format!(
                "clauth note: session resumed under `{now}`; earlier turns ran under `{before}`."
            ),
            Note::Switched { from, to } => format!(
                "clauth note: the active profile for this session switched from `{from}` to `{to}`."
            ),
        }
    }
}

/// The fields of a hook payload this subcommand reads; everything else is
/// ignored.
struct Payload {
    /// Echoed back in the output envelope, so the host routes the context to the
    /// event it came from.
    event: String,
    session_id: String,
    /// Present only on a fire from inside a subagent, which is what makes it the
    /// per-call scope key.
    agent_id: Option<String>,
    /// `SessionStart` only. Claude Code documents five: `startup`, `resume`,
    /// `clear`, `compact`, `fork`. Anything this does not recognise rebaselines
    /// silently, because every source Claude Code has added so far marks a
    /// context boundary, and announcing a switch about turns a fresh context
    /// never held is the worse failure.
    source: Option<String>,
    /// Recorded so the sweep can reap a record whose conversation is gone.
    transcript: Option<PathBuf>,
}

/// One scope's memory of what it was last told, plus the cache that lets the
/// common fire answer without resolving anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct NoteRecord {
    /// The account this scope was last told about. `None` until a first fire
    /// establishes the baseline — there are no earlier turns to correct then.
    #[serde(default)]
    told: Option<String>,
    /// The note last emitted to this scope. Compaction drops injected context
    /// while `told` would suppress a second note, so the stored text is what a
    /// `source: "compact"` fire re-announces.
    #[serde(default)]
    last_note: Option<String>,
    /// Stamp of the resolution's inputs when `resolved` was taken.
    ///
    /// Written ONLY when the resolution attributed an account. Caching a `None`
    /// here would bank the very stamp move that opened the gate, and nothing
    /// moves it again — so the note would be lost rather than deferred, for the
    /// life of the conversation. An ordinary rotation reaches that: it writes
    /// the live file (stamped) and then the profile store (not).
    #[serde(default)]
    watch: Option<Watch>,
    /// What the last resolution answered, cached behind `watch` + [`resolved_at`].
    /// Never `Some(None)` in effect: an unattributed read is not cached at all.
    #[serde(default)]
    resolved: Option<String>,
    /// When `resolved` was taken, for [`RESOLUTION_TTL`].
    #[serde(default)]
    resolved_at: Option<SystemTime>,
    /// This conversation's transcript, for the sweep.
    #[serde(default)]
    transcript: Option<PathBuf>,
}

impl NoteRecord {
    /// Whether the record already holds an observation at least as new as
    /// `taken_at` — and one taken in the PAST.
    ///
    /// Both halves earn their place. Without the first, a fire that resolved
    /// before a peer overwrites the fresher verdict and announces the reversal.
    /// Without the second, one backward clock step (chrony/timesyncd stepping a
    /// large offset, a VM snapshot restore, a suspend/resume — all of which land
    /// exactly when sessions start) leaves `resolved_at` in the future and every
    /// later fire defers to it, discarding correct answers for the size of the
    /// step. [`RESOLUTION_TTL`] cannot bound that: this runs on the path
    /// [`cache_holds`] has already rejected, which is why the fire resolved.
    ///
    /// `>=` rather than `>`: on a tie both fires would otherwise cache and the
    /// later ARRIVER would win regardless of who observed first (measured at
    /// ~0.0095% of simultaneous stamp pairs). A fire stamps exactly once, so it
    /// can never tie with its own prior write.
    fn holds_a_newer_observation_than(&self, taken_at: SystemTime) -> bool {
        self.resolved_at
            .is_some_and(|held| held >= taken_at && held <= SystemTime::now())
    }

    /// Whether the cached resolution still answers for `watch`: an account was
    /// attributed, the stamped inputs have not moved, and the answer is younger
    /// than [`RESOLUTION_TTL`]. All three, because the stamp alone has been
    /// measured to miss an input and a miss it cannot see is unbounded.
    fn cache_holds(&self, watch: &Watch) -> bool {
        self.resolved.is_some()
            && self.watch.as_ref() == Some(watch)
            && self.resolved_at.is_some_and(|at| {
                SystemTime::now()
                    .duration_since(at)
                    .is_ok_and(|age| age < RESOLUTION_TTL)
            })
    }
}

/// The inputs the attributed account is taken from, as far as a stat can see
/// them. Deliberately not a complete account of `load_config`'s reads — see
/// [`RESOLUTION_TTL`] for what bounds the remainder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Watch {
    /// The credential store this conversation's Claude Code loads. Followed
    /// through the link, since a swap repoints it and stamps the TARGET.
    creds: Option<Stamp>,
    /// Hash of [`crate::profile::reload_fingerprint`], the crate's own predicate
    /// for "could a config reload change the answer". It covers `profiles.toml`
    /// AND every per-profile `config.toml` and `session-token.json` — the ones a
    /// hand-rolled pair of stats missed, which let a `disabled = true` flip
    /// change the attributed account behind a closed gate.
    ///
    /// Hashed rather than stored whole because the record is JSON and the
    /// fingerprint is not a serde type. A hasher change across releases shifts
    /// every stored value at once, which opens the gate one extra time per
    /// conversation and costs one resolution.
    config: u64,
}

/// Mtime and length of one watched file, `None` when it is absent. Length rides
/// along because an mtime alone cannot separate two writes a coarse filesystem
/// truncates into one tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    mtime: SystemTime,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        mtime: meta.modified().ok()?,
        len: meta.len(),
    })
}

/// Stamp both inputs. Fails soft: an unresolvable credential path contributes
/// `None`, which compares equal to itself and so gates exactly like an absent
/// file. `reload_fingerprint` fails soft on its own terms (a stat error
/// contributes the empty value).
fn watch_now() -> Watch {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    crate::profile::reload_fingerprint().hash(&mut hasher);
    Watch {
        creds: crate::which::active_credentials_path()
            .as_deref()
            .and_then(stamp),
        config: hasher.finish(),
    }
}

/// The account this conversation's credentials resolve to, through the same tier
/// walk `clauth which` uses.
fn resolve_account() -> Option<String> {
    let config = crate::profile::load_config().ok()?;
    crate::which::resolve_active(&config).map(|(name, _)| name)
}

pub(crate) fn run() -> Result<()> {
    let mut input = String::new();
    // Bounded, not because a hostile payload is expected but because the host
    // supplies it and an unbounded read has no ceiling but RAM. A truncated
    // payload fails to parse and the fire goes silent, which is the same
    // outcome as any other malformed input.
    let _ = std::io::stdin()
        .take(MAX_PAYLOAD_BYTES)
        .read_to_string(&mut input);
    let Some(payload) = parse_payload(&input) else {
        return Ok(());
    };
    if let Some(note) = note_for(&payload, &watch_now(), &resolve_account) {
        emit(&payload.event, &note);
    }
    Ok(())
}

fn emit(event: &str, note: &str) {
    outln!("{}", envelope(event, note));
}

/// The hook's output payload, split from the print so its field shapes are
/// assertable without capturing stdout.
///
/// `hookEventName` is echoed from the payload rather than chosen here, so the
/// host routes the context back to the event that produced it. The note itself
/// reads as fact and never as an instruction: Claude Code's injection defenses
/// surface command-style text to the user instead of feeding it to the model.
fn envelope(event: &str, note: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": note,
        }
    })
}

fn parse_payload(input: &str) -> Option<Payload> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    if !is_bare_id(&session_id) {
        return None;
    }
    // Keyed on the field being ABSENT, never on `as_str()` succeeding. A
    // present-but-unusable value (a number, a bool, an object, or a string that
    // cannot spell a filename) belongs to a subagent whose scope cannot be
    // named, and treating it as absent consumes the main thread's record — the
    // one scope it must never touch. `as_str()` alone read a `12345` as absent.
    let agent_id = match value.get("agent_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(present) => match present.as_str() {
            Some(id) if is_bare_id(id) => Some(id.to_string()),
            _ => return None,
        },
    };
    // The event name is echoed into the envelope, so it is bounded. Unbounded, a
    // 1 MB `hook_event_name` came back as 1 MB on stdout.
    let event = value.get("hook_event_name")?.as_str()?;
    if !is_echoable_event(event) {
        return None;
    }
    Some(Payload {
        event: event.to_string(),
        session_id,
        agent_id,
        source: value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        transcript: value
            .get("transcript_path")
            .and_then(serde_json::Value::as_str)
            // Absolute and non-empty, or it is not a path the SWEEP can test for
            // liveness. `Path::new("").exists()` is false, which reaped live
            // records; a relative one resolves against the sweeping process's
            // cwd (a daemon, a `clauth start`), never the hook's.
            .filter(|p| !p.is_empty() && Path::new(p).is_absolute())
            .map(PathBuf::from),
    })
}

/// Whether `s` can spell a path COMPONENT on its own. Both ids arrive in a
/// payload this process does not author and both reach a filename, so each is
/// checked before any join.
fn is_bare_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `s` is safe to echo back as `hookEventName`.
///
/// Deliberately looser than [`is_bare_id`], and separate from it, because this
/// value never reaches a filename — it only has to be bounded and free of
/// anything that could break the envelope for a reader. Sharing the id charset
/// would take the hook silently offline for any event Claude Code ever
/// namespaces (`a.b`, `a:b`), with the failure looking like the feature simply
/// not firing.
fn is_echoable_event(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.chars().any(char::is_control)
}

fn records_dir() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join(RECORDS_DIR))
}

/// One record per (conversation, scope). The `.` separator is what keeps the two
/// shapes apart: [`is_bare_id`] admits no dot, so a subagent's file can never
/// spell the bare conversation's.
fn record_path(session_id: &str, agent_id: Option<&str>) -> Result<PathBuf> {
    let name = match agent_id {
        Some(agent) => format!("{session_id}.{agent}.json"),
        None => format!("{session_id}.json"),
    };
    Ok(records_dir()?.join(name))
}

fn load_record(path: &Path) -> Option<NoteRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Owner-only like every `~/.clauth` write: a record names the account a
/// conversation runs on and where its transcript sits.
fn store_record(path: &Path, record: &NoteRecord) -> Result<()> {
    atomic_write_600(path, serde_json::to_vec(record)?)?;
    Ok(())
}

/// The account a conversation's main scope was last told about — the durable
/// `told` baseline the hook maintains. This is the read the `delegate` resume
/// inference takes: `resolved` is deliberately not it, being a TTL-bounded
/// cache that only ever holds the last ATTRIBUTED answer, while `told` is the
/// baseline a conversation carries across processes.
///
/// `None` when the id cannot name a record (the hook only ever writes records
/// for bare ids, so a path- or dot-shaped id has none by construction), when
/// no record exists, or when the record never established a baseline. A plain
/// file read under no lock: writers replace the record atomically (temp +
/// rename), so a racing read parses the old or the new bytes whole, and a
/// failed parse answers `None` rather than a wrong account.
pub(crate) fn told_account(session_id: &str) -> Option<String> {
    // The id reaches a filename in `record_path`, so it is checked at this
    // boundary the same way the hook checks the one in its own payload.
    if !is_bare_id(session_id) {
        return None;
    }
    let path = record_path(session_id, None).ok()?;
    load_record(&path)?.told
}

/// Decide what this fire says and store what it learned.
///
/// `resolve` is taken by reference so a test can count how often the gate lets it
/// through; nothing else varies it.
fn note_for(
    payload: &Payload,
    watch: &Watch,
    resolve: &dyn Fn() -> Option<String>,
) -> Option<String> {
    let path = record_path(&payload.session_id, payload.agent_id.as_deref()).ok()?;

    // Peek UNLOCKED, only to decide whether the slow half is needed. `resolve`
    // goes through `load_config`, which chmod-walks the whole `~/.clauth` tree,
    // and that must never run inside the hold below.
    let fresh = match load_record(&path) {
        Some(peek) if peek.cache_holds(watch) => None,
        // Stamped BEFORE the resolve, never after it and never at write time.
        // Both later instants overstate the observation: the lock wait sits
        // after the resolve, and the resolve itself is milliseconds during
        // which a peer can observe and land.
        //
        // NOT a total guarantee, and do not read it as one. `taken_at` means
        // "when I started looking" and is only a PROXY for "when I looked". It
        // is right for the shape that matters — a fire that started earlier and
        // finished later now defers — but inverts when two fires start together
        // and read opposite sides of a switch landing inside their resolve
        // windows, where the staler reading can carry the later stamp and still
        // announce the reversal. Measured only as a MECHANISM, since the rate a
        // harness reports for it tracks its own spawn order rather than
        // production. The exposure is the resolve window (~1.5-3 ms) against the
        // up-to-2 s lock wait this replaced, and it self-corrects at the TTL.
        // Closing it means stamping inside `resolve_account` around the
        // credential read, which is more invasive than the residual deserves.
        _ => {
            let taken_at = SystemTime::now();
            Some((resolve(), taken_at))
        }
    };

    let _hold = ScopeLock::acquire();
    // Re-read INSIDE the hold. The peek above may be another writer's stale
    // bytes: a scope is not one writer, since every main-thread parallel tool
    // call fires with no `agent_id` and lands here.
    let stored = load_record(&path);
    let mut record = stored.clone().unwrap_or_else(|| NoteRecord {
        // A scope with no record of its own inherits the conversation's
        // baseline. A fresh `told` would adopt the CURRENT account as this
        // scope's first observation, so a subagent firing after the change would
        // silently eat the note instead of hearing it.
        told: inherited_baseline(payload),
        ..NoteRecord::default()
    });
    if payload.transcript.is_some() {
        record.transcript = payload.transcript.clone();
    }
    let current = match fresh {
        // The cache still answers, and the copy under the lock outranks the peek.
        None => record.resolved.clone(),
        // A fire whose observation PREDATES the one already recorded is carrying
        // the staler reading, whatever order the two reached the lock in:
        // resolving happens outside the hold, so arrival order says nothing
        // about observation order, and the 2 s lock wait widens that gap rather
        // than closing it. Defer to the record, exactly as the cache-hit branch
        // above does. Overwriting instead let a slow fire announce the reversal
        // (`switched from cld to kerry` for a switch that never happened) and
        // cache its stale answer for the whole TTL.
        Some((_, taken_at)) if record.holds_a_newer_observation_than(taken_at) => {
            record.resolved.clone()
        }
        Some((answer, taken_at)) => {
            // Only an ATTRIBUTED answer is cached. See `NoteRecord::watch`.
            if answer.is_some() {
                record.watch = Some(watch.clone());
                record.resolved = answer.clone();
                record.resolved_at = Some(taken_at);
            }
            answer
        }
    };
    let note = decide(payload, &mut record, current.as_deref());
    if stored.as_ref() != Some(&record) && store_record(&path, &record).is_err() {
        // The record IS the suppression mechanism, so a note that cannot be
        // remembered is re-emitted on every tool call for the life of the
        // conversation. Keyed on the write failing at all rather than on any one
        // cause: a full disk and a read-only tree reach this the same way.
        // The log FILE, not `logline!`. This runs once per tool call, so a
        // persistent failure through the routed sink lands on a hook's
        // (non-terminal) stderr once per fire — the same unbounded flood this
        // suppression exists to prevent, moved onto the channel Claude Code
        // shows the user. The file is size-rotated; stderr is not.
        crate::logline::to_logfile(format_args!(
            "hook-note: cannot persist {}; staying silent",
            path.display()
        ));
        return None;
    }
    note
}

/// An exclusive hold over the records dir for one read-modify-write.
///
/// A LEAF in the lock order: nothing is acquired while it is held, and the
/// resolution that would reach `~/.clauth`'s own state lock runs before it. One
/// lock file for the whole dir rather than one per scope, because the hold is a
/// read plus a rename and the only contention is a fan-out's own fires, so
/// per-scope granularity would buy nothing and add a second file to reap.
///
/// Failing to take it degrades to the pre-lock behaviour (a possible duplicate
/// note) rather than to silence: a hook must not block a tool call on a lock.
/// The deadline is also what keeps a NESTED acquisition soft — `flock` blocks a
/// second fd in the same process, so a future caller that takes this around
/// something already holding it degrades after the wait instead of hanging.
/// Today there is no such nesting: `note_for` and `gc_conversation_records` are
/// the only holders and neither reaches the other.
struct ScopeLock {
    /// Held open for the guard's lifetime and never read: closing the fd is what
    /// releases the flock, so the binding IS the lock. Named like `StateLock`'s
    /// own guards for the same reason.
    _held: Option<std::fs::File>,
}

impl ScopeLock {
    fn acquire() -> Self {
        const WAIT: Duration = Duration::from_secs(2);
        let held = (|| {
            let dir = records_dir().ok()?;
            crate::profile::mkdir_700(&dir).ok()?;
            let file = crate::profile::open_state_file(&dir.join(".lock")).ok()?;
            if let Err(e) = crate::lock::lock_file_with_timeout(&file, WAIT) {
                // Never swallowed: proceeding unlocked is duplicate notes
                // coming back, and without this the only diagnostic that
                // exists is discarded and the degradation is silent.
                crate::logline::to_logfile(format_args!(
                    "hook-note: proceeding without the scope lock: {e}"
                ));
                return None;
            }
            Some(file)
        })();
        Self { _held: held }
    }
}

/// What a scope firing for the first time treats as its starting account: the
/// main thread's, when this is a subagent. The main thread has no one to inherit
/// from, so its own first fire is the baseline.
fn inherited_baseline(payload: &Payload) -> Option<String> {
    payload.agent_id.as_ref()?;
    let main = record_path(&payload.session_id, None).ok()?;
    load_record(&main)?.told
}

/// The change test, against the record this scope carries. `current` is `None`
/// when clauth cannot attribute the loaded credentials.
fn decide(payload: &Payload, record: &mut NoteRecord, current: Option<&str>) -> Option<String> {
    // An unattributable credential is not evidence that anything moved: a
    // disabled profile, a `claude login` clauth holds no copy of, and a config
    // it could not parse all land here. Leaving `told` standing is what keeps a
    // later real move rendering both real names instead of one and a shrug.
    let current = current?;
    if payload.event == "SessionStart" {
        match payload.source.as_deref() {
            Some("resume") => {
                return match record.told.as_deref() {
                    Some(before) if before != current => {
                        let note = Note::Resumed {
                            now: current,
                            before,
                        }
                        .render();
                        Some(tell(record, current, note))
                    }
                    Some(_) => {
                        // Same account across the restart. Drop whatever the
                        // PREVIOUS process emitted, or a later compaction
                        // re-announces a switch belonging to a process this
                        // context never saw — and re-announces it every time.
                        //
                        // Rests on an UNVERIFIED premise: that hook-injected
                        // `additionalContext` is not replayed into the resumed
                        // context. If Claude Code does replay it, the context
                        // did see that note and re-announcing was right. Nobody
                        // has measured which; the behaviour is pinned either way
                        // by `a_resume_on_the_same_account_drops_the_previous_processes_note`.
                        record.last_note = None;
                        None
                    }
                    None => {
                        record.told = Some(current.to_string());
                        None
                    }
                };
            }
            // Compaction dropped whatever was injected, while the record would
            // suppress a second note — so without this a conversation that
            // compacts after a change is left believing the old account.
            Some("compact") => {
                return match record.told.as_deref() {
                    Some(before) if before != current => {
                        let note = Note::Switched {
                            from: before,
                            to: current,
                        }
                        .render();
                        Some(tell(record, current, note))
                    }
                    Some(_) => record.last_note.clone(),
                    None => {
                        // A compaction before anything was ever told. There is
                        // nothing to re-announce, and returning without setting
                        // `told` would leave the scope baseline-less for another
                        // fire.
                        record.told = Some(current.to_string());
                        None
                    }
                };
            }
            // `startup`, `clear`, `fork`, and anything Claude Code adds later.
            // A fresh context holds no earlier turns to correct, and every
            // source added so far marks a context boundary — so an unrecognised
            // one rebaselines rather than announcing a switch about turns that
            // never existed.
            _ => {
                record.told = Some(current.to_string());
                record.last_note = None;
                return None;
            }
        }
    }
    match record.told.as_deref() {
        Some(before) if before != current => {
            let note = Note::Switched {
                from: before,
                to: current,
            }
            .render();
            Some(tell(record, current, note))
        }
        Some(_) => None,
        None => {
            record.told = Some(current.to_string());
            None
        }
    }
}

/// Record `note` as this scope's newest and hand it back. One place, so `told`
/// and `last_note` cannot advance apart.
fn tell(record: &mut NoteRecord, account: &str, note: String) -> String {
    record.told = Some(account.to_string());
    record.last_note = Some(note.clone());
    note
}

/// Drop the records of conversations that are gone, from the same sweep that
/// reaps stale runtime trees and registry rows.
///
/// A record names its own transcript, so the test is exact rather than an age
/// guess: Claude Code deleting the transcript is the conversation ending for
/// good. The age clause below covers only a record that never carried one.
pub(crate) fn gc_conversation_records() {
    let Ok(dir) = records_dir() else {
        return;
    };
    // Peek BEFORE locking, the way `gc_bare_markers` does and for the same
    // reason: this runs at every `clauth mcp` boot and every `clauth start`, so
    // nothing to sweep must not pay an acquisition. It also keeps the
    // acquisition's `mkdir_700` off a box where the hook has never fired, which
    // would otherwise grow a records dir and a lock file from a sweep alone.
    //
    // The early return covers a VIRGIN tree only: `.lock` is permanent once any
    // hook has fired, so a box with zero records still counts one entry and
    // still pays. Sub-ms uncontended, and the same shape the sibling has.
    let Ok(mut peek) = std::fs::read_dir(&dir) else {
        return;
    };
    if peek.next().is_none() {
        return;
    }
    // Under the same hold the writers take. Without it the sweep unlinks the
    // very files `ScopeLock` serialises, so a reap landing inside a fire's
    // read-modify-write drops that write on the floor.
    //
    // What the lock does NOT cover, measured rather than reasoned: the reap
    // costs a live scope its baseline with ZERO concurrency involved
    // (baseline, move, sweep, fire — the fire finds no record and rebaselines,
    // swallowing that one account change). 40 concurrent trials against a
    // fresh record announced 40/40; against a reap-eligible one, 0/40. So the
    // loss is caused by the reap predicate and not by any interleave, and the
    // deletion is self-undoing — the fire recreates the record immediately. The
    // real guard belongs on the predicate; this lock was never
    // going to cover it.
    let _hold = ScopeLock::acquire();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Records only. The dir also holds the `.lock` file and, for an instant,
        // an `atomic_write_600` temp; reaping either would be a sweep deleting
        // live machinery.
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let age = || {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
        };
        let reap = match load_record(&path).and_then(|r| r.transcript) {
            // Grace, not a bare `!exists()`: a baseline recorded at
            // `SessionStart` can land before Claude Code creates the transcript,
            // and reaping it there loses the baseline, so the conversation's next
            // real move is absorbed as a first fire and never announced.
            Some(transcript) => {
                !transcript.exists() && age().is_some_and(|a| a > MISSING_TRANSCRIPT_GRACE)
            }
            None => age().is_some_and(|a| a > ORPHAN_RECORD_MAX_AGE),
        };
        if reap {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/hook_note.rs"]
mod tests;
