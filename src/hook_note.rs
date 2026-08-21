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
//! - **A stat gates the resolution.** This runs on every tool call, so the
//!   record carries the stamp of the two inputs the answer is taken from and
//!   skips the resolution behind them when neither moved. The measured spawn
//!   floor is ~1.25 ms (20 `clauth --version` spawns in 25 ms, 2026-08-20) and
//!   the gate is what keeps a real subcommand near it.
//! - **One record per SCOPE, not per conversation.** A `PostToolUse` fired
//!   inside a subagent carries `agent_id`; a single shared told-flag would let
//!   the first subagent to fire consume the note while the main thread never
//!   hears it. Separate files also mean the concurrent fires of a fan-out never
//!   read-modify-write the same bytes, so no lock is needed for it.
//!
//! Every failure is silence at exit 0. A hook that errors on a tool call breaks
//! the conversation it exists to inform.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::out::outln;
use crate::profile::atomic_write_600;

/// Dir under `~/.clauth` holding one record per conversation scope.
const RECORDS_DIR: &str = "conversations";

/// How long a record with no transcript to test it by survives the sweep. Only
/// a payload that carried no `transcript_path` lands here, so this is the
/// backstop rather than the rule.
const ORPHAN_RECORD_MAX_AGE: std::time::Duration =
    std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// The two spellings, behind one renderer so they cannot drift apart.
///
/// The noun is "conversation" and never "session": every existing "session" in
/// clauth's model-facing copy names the process, while this names the transcript
/// that outlives it.
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
                "clauth note: the active profile for this conversation switched from `{from}` to `{to}`."
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
    /// `SessionStart` only: `startup | resume | clear | compact`.
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
    /// Stamp of the resolution's inputs when `resolved` was taken. `None` means
    /// nothing has been resolved yet, which is what keeps a cached `resolved` of
    /// `None` distinguishable from never having asked.
    #[serde(default)]
    watch: Option<Watch>,
    /// What the last resolution answered, cached behind `watch`.
    #[serde(default)]
    resolved: Option<String>,
    /// This conversation's transcript, for the sweep.
    #[serde(default)]
    transcript: Option<PathBuf>,
}

/// The inputs the attributed account is taken from. Both are stat'd, never read:
/// the point is to skip the read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Watch {
    /// The credential store this conversation's Claude Code loads. Followed
    /// through the link, since a swap repoints it and stamps the TARGET.
    creds: Option<Stamp>,
    /// `profiles.toml` — which account is active and which accounts exist. It is
    /// what moves when a switch lands between two accounts that store no
    /// credentials of their own, where the file above is absent either side.
    state: Option<Stamp>,
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

/// Stat both inputs. Fails soft: an unresolvable path contributes `None`, which
/// compares equal to itself and so gates exactly like an absent file.
fn watch_now() -> Watch {
    Watch {
        creds: crate::which::active_credentials_path()
            .as_deref()
            .and_then(stamp),
        state: crate::profile::app_state_path()
            .ok()
            .as_deref()
            .and_then(stamp),
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
    let _ = std::io::stdin().read_to_string(&mut input);
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
    let agent_id = match value.get("agent_id").and_then(serde_json::Value::as_str) {
        // An agent id that cannot spell a filename must not fall back to the
        // main thread's record: that is the one scope it would then consume.
        Some(id) if !is_bare_id(id) => return None,
        Some(id) => Some(id.to_string()),
        None => None,
    };
    Some(Payload {
        event: value.get("hook_event_name")?.as_str()?.to_string(),
        session_id,
        agent_id,
        source: value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        transcript: value
            .get("transcript_path")
            .and_then(serde_json::Value::as_str)
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
    let current = if record.watch.as_ref() == Some(watch) {
        record.resolved.clone()
    } else {
        let resolved = resolve();
        record.watch = Some(watch.clone());
        record.resolved = resolved.clone();
        resolved
    };
    let note = decide(payload, &mut record, current.as_deref());
    if stored.as_ref() != Some(&record) {
        let _ = store_record(&path, &record);
    }
    note
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
            // A fresh or cleared context carries no earlier turns to correct.
            Some("startup" | "clear") => {
                record.told = Some(current.to_string());
                record.last_note = None;
                return None;
            }
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
                    Some(_) => None,
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
                    _ => record.last_note.clone(),
                };
            }
            _ => {}
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
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let reap = match load_record(&path).and_then(|r| r.transcript) {
            Some(transcript) => !transcript.exists(),
            None => entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .is_some_and(|age| age > ORPHAN_RECORD_MAX_AGE),
        };
        if reap {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/hook_note.rs"]
mod tests;
