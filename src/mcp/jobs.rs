//! Disk-backed job store for background `delegate` calls.
//!
//! A background delegate returns a `job_id` at once and finishes on a detached
//! blocking task. The result must outlive the originating tool call AND be
//! readable by a separate process (the `mcp-await-job` PostToolUse hook), so it
//! lands on disk at `~/.clauth/jobs/<job_id>.json` rather than an in-memory
//! registry. Writes are atomic (tmp + rename) so a concurrent reader never sees a
//! torn file. No lock is taken: the path is keyed by a unique `job_id` and the
//! finalizing task is the sole writer for its own file — a leaf with no ordering
//! against the runtime/state locks.

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
/// exposures are exactly what the age rule carried, and neither is widened here.
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
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) last_output_at: u64,
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
    pub(crate) timeout_secs: u64,
    pub(crate) idle_secs: Option<u64>,
}

pub(crate) fn jobs_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("jobs"))
}

/// A fresh, process-unique, filesystem-safe job id: `started_at` (epoch ms) plus
/// a monotonic counter.
pub(crate) fn new_job_id(started_at: u64) -> String {
    let n = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("d-{started_at}-{n}")
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
/// `launch_background_delegate` calls `write_done` only after `run_delegate`
/// returns. `run_delegate_never_returns_between_spawning_the_reader_and_joining_it`
/// is what holds the single-exit half of that up, since a `return` in between
/// would orphan a thread that then overwrites the finalized record.
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
fn retention_anchor(record: &JobRecord) -> u64 {
    match record.state {
        JobState::Done => {
            if record.done_at > 0 {
                record.done_at
            } else {
                record.started_at
            }
        }
        JobState::Running => record.last_output_at.max(record.started_at),
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
