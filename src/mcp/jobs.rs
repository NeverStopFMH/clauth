//! Disk-backed job store for background `delegate` calls.
//!
//! A background delegate returns a `job_id` at once and finishes on a detached
//! blocking task. The result must outlive the originating tool call AND be
//! readable by a separate process (the `mcp-await-job` PostToolUse hook), so it
//! lands on disk at `~/.clauth/jobs/<job_id>.json` rather than an in-memory
//! registry. Writes are atomic (tmp + rename) so a concurrent reader never sees
//! a torn file. No lock is taken: the path is keyed by a unique `job_id` and the
//! finalizing task is the sole writer for its own file — a leaf with no ordering
//! against the runtime/state locks.
//!
//! A BLOCKING delegate whose caller walks away also ends up here
//! (`Handoff::hand_off` mints it a record mid-run), and it writes the same file
//! through the same code — but it is NOT delivered the same way, and the
//! paragraph above does not reach it. Measured on Claude Code 2.1.233: a tool
//! call the client cancelled or timed out dispatches `PostToolUseFailure`, never
//! `PostToolUse`, so the bundled hook never runs for it; and the reply carrying
//! the minted id is dropped by rmcp before it reaches the transport, so the
//! model never learns the id to ask for it. Such a record is written so the
//! spent window's result EXISTS rather than because anything currently collects
//! it: today it is reachable by an operator reading the store, and by `monitor`
//! only from an id learned some other way.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::profile::clauth_dir;

/// Retain a `done` file this long AFTER IT FINISHES before GC reaps it; long
/// enough that a slow poller can still collect a result the auto-delivery hook
/// already delivered. Measured from `done_at`, not from the mint: a delegate
/// that ran for hours is already hours old the instant it finalizes, so a
/// mint-anchored TTL would expire every long run's salvage envelope before
/// anyone could read it.
pub(super) const DONE_TTL_MS: u64 = 60 * 60 * 1000; // 1h
/// A `running` file SILENT this long is orphaned (its server died mid-job); reap
/// it.
///
/// Silence rather than age, because a streaming delegate has no wall clock and
/// so no maximum lifetime to sit above: a run still healthy at any age would
/// have had its file deleted under it, and answered `unknown job_id` while its
/// child kept spending the account.
///
/// "Silent" is measured from the record's own mint (`recorded_at`), not the
/// run's birth. A blocking delegate handed off mid-flight keeps a `started_at`
/// from arbitrarily long before its file existed, so anchoring on that would
/// mint a two-hour run already expired — and a pinned-format one, which never
/// heartbeats at all, would be reaped by every reader for the rest of its life.
///
/// The two background shapes reach 3600 + 600 s by different routes, and only
/// the first one heartbeats:
///
/// - **Streaming.** `read_stdout` rewrites this file on every line it consumes,
///   so the record's own stamp tracks the run, and the supervision loop kills it
///   once it has been quiet for `idle_secs` — caller-set, clamped to
///   `MAX_RUN_TIMEOUT_SECS` (3600 s). 600 s of slack covers the heartbeat
///   throttle, the kill and the teardown before `write_done` lands.
/// - **Pinned `--output-format`.** `read_stdout` drains the pipe whole and never
///   consults the heartbeat sink, so `last_output_at` stays `0` for this run's
///   entire life and the anchor is its mint. What bounds it is the wall clock it
///   always has (also ≤ 3600 s), not any liveness it emits.
///
/// So the silent window this must cover is the pre-spawn delay for a streaming
/// run, and the pre-spawn delay PLUS the whole run for a pinned-format one. Both
/// can overrun it, from the same cause — `ProfileRuntime::acquire` blocks behind
/// a `clauth start` session on the same profile, for as long as that session
/// lasts — and they differ in what is alive when the reap lands: a streaming run
/// is still inside that acquire with no child, while a pinned-format one can be
/// well past it, since a 700 s block plus its 3600 s wall already puts the record
/// 4300 s silent-since-mint with the child 3600 s in and still spending. Both
/// exposures are exactly what the age rule carried. A handed-off run adds no
/// third one: its clock starts at the crossing, which is strictly after the
/// spawn, so it is bounded by whichever of the two shapes it already is.
const RUNNING_TTL_MS: u64 = (3600 + 600) * 1000;
/// Hard cap on retained job files; newest kept, older reaped. Also the cap on
/// one `monitor` `job_ids` list: the store holds at most this many files, so a
/// longer list could not resolve more ids.
pub(crate) const MAX_RETAINED: usize = 256;

/// Per-process counter making two job ids minted in the same millisecond differ.
static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JobState {
    Running,
    Done,
}

/// `#[serde(skip_serializing_if)]` predicate for a numeric field at its default.
fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobRecord {
    pub(crate) job_id: String,
    pub(crate) profile: String,
    pub(crate) state: JobState,
    pub(crate) started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) envelope: Option<serde_json::Value>,
    /// The wall-clock ceiling this run actually launched under, resolved once by
    /// `resolve_deadlines`. `0` is never a run about to be killed: it means this
    /// run HAS no wall clock, which is the normal streaming case, or — paired
    /// with an absent `idle_secs` — that the server which wrote the record
    /// predates these fields. `idle_secs` is what tells those two apart.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) timeout_secs: u64,
    /// The idle ceiling, `None` when the idle leg is off entirely (a
    /// caller-pinned `--output-format` leaves silence carrying no information).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idle_secs: Option<u64>,
    /// Epoch ms of the most recent stdout line — the same anchor `started_at`
    /// uses, so a reader subtracts them with no error term. `0` = nothing has
    /// arrived yet. (A run-relative stamp would be anchored at the child's
    /// spawn, which trails the mint by the config load, the pre-flight and the
    /// runtime acquire.)
    ///
    /// It is NOT the retention anchor's floor — see [`recorded_at`]. Stamping
    /// this at a mint to hold a record alive would buy that with a false
    /// liveness claim: it renders as `last_output_secs_ago` and it is what
    /// `idle_kill_in_secs` counts from, so a run silent for 280 s of its 300 s
    /// idle guard would report a full 300 s of headroom moments before the
    /// supervision loop killed it.
    ///
    /// [`recorded_at`]: Self::recorded_at
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) last_output_at: u64,
    /// Epoch ms this RECORD was written, which is not always when its RUN
    /// started: a blocking delegate handed off mid-flight (`Handoff::hand_off`)
    /// keeps the run's real `started_at`, because `elapsed_secs` and the job id
    /// are derived from it, while its file has existed only since the crossing.
    ///
    /// [`retention_anchor`] needs the later of the two, or a run handed off
    /// after two hours is minted already past [`RUNNING_TTL_MS`] and the very
    /// next `monitor` reaps the record it came to read. `0` on a file written
    /// before this field existed, where `started_at` was the mint and the
    /// fallback is exact.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) recorded_at: u64,
    /// A bounded single-line tail of the delegate's assistant text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) tail: String,
    /// Epoch ms the job finalized, which is what [`DONE_TTL_MS`] retains from.
    /// `0` on a `running` record and on a `done` file an older server wrote,
    /// where [`gc`] falls back to the mint and so keeps exactly its old
    /// behaviour.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) done_at: u64,
}

/// What a background job's `running` record carries from its reserve through
/// every heartbeat: identity plus the deadlines the run launched under. Grouped
/// so the reserve resolves them once and the heartbeat cannot re-derive them
/// differently — `resolve_deadlines` applies defaults, clamps and a streaming
/// fork, and a second derivation goes wrong the first time that fork changes.
#[derive(Debug, Clone)]
pub(crate) struct RunningSpec {
    pub(crate) job_id: String,
    pub(crate) profile: String,
    pub(crate) started_at: u64,
    /// When the record was minted; equal to `started_at` for a job that started
    /// out background, later for one handed off mid-run. Carried through every
    /// heartbeat, since a beat rewrites the whole record and would otherwise
    /// drop it back to the run's birth.
    pub(crate) recorded_at: u64,
    pub(crate) timeout_secs: u64,
    pub(crate) idle_secs: Option<u64>,
}

pub(crate) fn jobs_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("jobs"))
}

/// Lowercase base-36 digits for [`base36`], ordered by value so `n % 36`
/// indexes straight into it.
const B36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// The smallest base-36 spelling of `n`, `0` for zero. A `u64` needs at most 13
/// base-36 digits, so the fixed buffer never overruns.
fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut rev = [0u8; 13];
    let mut i = rev.len();
    while n > 0 {
        i -= 1;
        rev[i] = B36[(n % 36) as usize];
        n /= 36;
    }
    let mut out = String::with_capacity(rev.len() - i);
    for &b in &rev[i..] {
        out.push(char::from(b));
    }
    out
}

/// A fresh, process-unique, filesystem-safe job id: `started_at` (epoch ms) in
/// base-36, then a decimal monotonic counter. The stamp is encoded to keep the
/// id short; the counter stays decimal because a same-millisecond run count is
/// already tiny, and its only job is to differ.
pub(crate) fn new_job_id(started_at: u64) -> String {
    let n = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("d-{}-{n}", base36(started_at))
}

/// True iff `id` is safe as a single path component (no separators, no
/// traversal). Job ids reaching `monitor` / `mcp-await-job` come from
/// tool input, so this guards the path join.
pub(crate) fn is_safe_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn job_path(job_id: &str) -> Result<PathBuf> {
    Ok(jobs_dir()?.join(format!("{job_id}.json")))
}

/// Persist a record atomically (tmp + rename, so a reader sees either the old
/// file or the fully-written new one, never a torn write). Owner-only: a job
/// file carries the delegate's prompt and the account's full response, and lands
/// under `~/.clauth`, so it rides the 0o600 dir-0o700 invariant.
fn write_atomic(record: &JobRecord) -> Result<()> {
    let bytes = serde_json::to_vec(record)?;
    crate::profile::atomic_write_600(&job_path(&record.job_id)?, &bytes)?;
    Ok(())
}

/// Write the initial `running` record for a freshly-started background job:
/// the reserved spec with nothing observed yet.
/// `#[serde(default)]` on every later `JobRecord` field is what lets a job file
/// written by an older server still parse here.
pub(crate) fn write_running(spec: &RunningSpec) -> Result<()> {
    write_heartbeat(spec, 0, "")
}

/// Rewrite a running job's record with its freshest liveness: the epoch ms of
/// its last stdout line, and the bounded tail of what it has said.
///
/// Lock-free against [`write_done`] because the two cannot interleave: the
/// stdout reader thread is this function's only caller, and `run_delegate` joins
/// that thread on every exit path before it builds any envelope, while
/// `Handoff::finalize` — the sole `write_done` caller for a job — runs only
/// after `run_delegate` returns.
/// `run_delegate_never_returns_between_spawning_the_reader_and_joining_it`
/// is what holds the single-exit half of that up, since a `return` in between
/// would orphan a thread that then overwrites the finalized record.
///
/// A run handed off mid-flight does not widen that: the record it starts
/// heartbeating into is minted before its first beat resolves one, and the same
/// single reader thread does every beat either way.
pub(crate) fn write_heartbeat(spec: &RunningSpec, last_output_at: u64, tail: &str) -> Result<()> {
    write_atomic(&JobRecord {
        job_id: spec.job_id.clone(),
        profile: spec.profile.clone(),
        state: JobState::Running,
        started_at: spec.started_at,
        envelope: None,
        timeout_secs: spec.timeout_secs,
        idle_secs: spec.idle_secs,
        last_output_at,
        recorded_at: spec.recorded_at,
        tail: tail.to_string(),
        done_at: 0,
    })
}

/// Finalize a job: overwrite its file with the completed envelope, stamped with
/// the moment it finished — which is what [`DONE_TTL_MS`] retains from. The
/// running-only fields default away: a finished job has no deadline left to
/// count down to and no tail worth keeping beside its whole result.
pub(crate) fn write_done(
    job_id: &str,
    profile: &str,
    started_at: u64,
    envelope: serde_json::Value,
) -> Result<()> {
    write_atomic(&JobRecord {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        state: JobState::Done,
        started_at,
        envelope: Some(envelope),
        timeout_secs: 0,
        idle_secs: None,
        last_output_at: 0,
        recorded_at: 0,
        tail: String::new(),
        done_at: crate::usage::now_ms(),
    })
}

/// Read a job record, or `None` if the file is absent or unparseable.
pub(crate) fn read(job_id: &str) -> Option<JobRecord> {
    let bytes = std::fs::read(job_path(job_id).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Delete a job file (best-effort). Called after a fallback `monitor` collect
/// hands the envelope back.
pub(crate) fn remove(job_id: &str) {
    if let Ok(path) = job_path(job_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Best-effort GC at server startup: drop `done` files past their TTL and
/// `running` files silent past [`RUNNING_TTL_MS`] (orphaned by a dead server),
/// sweep stray `.tmp` from a crash mid-write, then cap the retained count to the
/// newest [`MAX_RETAINED`].
pub(crate) fn gc(now: u64) {
    sweep(now, Scope::Everything);
}

/// The narrower sweep a `monitor` collect runs: `running` files a dead server
/// orphaned, and nothing else.
///
/// A reader must never destroy what it came for. The Done TTL, the `.tmp` sweep
/// and the retention cap all buy nothing before a read and can only delete a
/// result the caller is asking for, so they stay at startup. What DOES belong
/// here is the corpse: [`RUNNING_TTL_MS`] already knows a file whose server died
/// mid-job is dead, and until now `serve()` was the only place that knowledge
/// was ever applied, so a corpse polled `running` forever.
pub(crate) fn gc_running_corpses(now: u64) {
    sweep(now, Scope::RunningCorpses);
}

/// How much of the store one sweep is allowed to touch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Everything,
    RunningCorpses,
}

/// The stamp both retention rules read: when this record last mattered. A `done`
/// record's finish, falling back to its mint for a file written before `done_at`
/// existed; a `running` record's freshest heartbeat, falling back to its mint
/// before the first line of output arrives.
///
/// One anchor for the TTL and the cap together, because they answer the same
/// question — which records are the stale ones — and mixing stamps is what the
/// TTL itself got wrong twice: sorted on the mint, the cap evicts a long
/// delegate's fresh, never-read result ahead of a short run's older one, and
/// [`RUNNING_TTL_MS`] reaped a live long run for having started a while ago.
///
/// A `Running` record takes the latest of its three stamps rather than the two
/// it used to, because a hand-off separated the run's birth from the record's:
/// on `started_at` alone, a delegate handed off after two hours is minted
/// already expired and the next reader sweeps it. `recorded_at` is `0` on a file
/// written before that field, where the mint WAS `started_at` and the pair
/// collapses back to the old rule exactly.
fn retention_anchor(record: &JobRecord) -> u64 {
    match record.state {
        JobState::Done => {
            if record.done_at > 0 {
                record.done_at
            } else {
                record.started_at
            }
        }
        JobState::Running => record
            .last_output_at
            .max(record.recorded_at)
            .max(record.started_at),
    }
}

fn sweep(now: u64, scope: Scope) {
    let full = scope == Scope::Everything;
    let Ok(dir) = jobs_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut kept: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            if full {
                let _ = std::fs::remove_file(&path); // stray tmp / foreign file
            }
            continue;
        }
        let record = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<JobRecord>(&b).ok());
        let Some(record) = record else {
            // A file this sweep cannot read might still be a result: only the
            // startup sweep, which owns the store, discards one.
            if full {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        };
        let anchor = retention_anchor(&record);
        let expired = match record.state {
            JobState::Done => full && now.saturating_sub(anchor) > DONE_TTL_MS,
            JobState::Running => now.saturating_sub(anchor) > RUNNING_TTL_MS,
        };
        if expired {
            let _ = std::fs::remove_file(&path);
        } else {
            kept.push((anchor, path));
        }
    }
    if full && kept.len() > MAX_RETAINED {
        // Sorted on the SAME anchor the TTL above reads: newest-mattering
        // kept, so a long delegate's fresh result is not evicted ahead of a
        // short run's older one just because it started earlier.
        kept.sort_by_key(|k| std::cmp::Reverse(k.0));
        for (_, path) in kept.into_iter().skip(MAX_RETAINED) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_jobs.rs"]
mod tests;
