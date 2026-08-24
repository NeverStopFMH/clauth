//! Best-effort herdr agents-panel state reports for the pane this MCP server
//! runs in (`herdr pane report-agent`).
//!
//! herdr injects `HERDR_PANE_ID` and `HERDR_BIN_PATH` into every pane process,
//! so the server (a Claude Code child) inherits its pane id. While a
//! `delegate` is in flight the pane's agent icon reports `working`; when the
//! last in-flight delegate ends it reports `idle`. One process-local counter
//! tracks sync and background delegates together, so a background job
//! finalizing mid-run can never clear the icon while a sync delegate still
//! runs.
//!
//! Every report is best-effort: a failed or hanging herdr spawn never fails a
//! delegate. It does cost time, and the serve runtime is
//! `new_current_thread()`, so the cost is the whole server's. Only the
//! `working` half is charged to that thread now — it runs at commit-to-launch,
//! before the delegate reaches `spawn_blocking` — while `idle` rides the run's
//! own blocking task, where a hung herdr delays nothing but that task's own
//! end. Failures are silent except for one `logline`
//! each (the MCP stdio channel carries only the JSON-RPC frame on stdout, and
//! `logline` routes off it — to the log file on an interactive pane, stderr
//! otherwise).
//!
//! Gating: [`PaneReporter::resolve`] returns `None` unless BOTH `HERDR_PANE_ID`
//! is present AND the herdr binary resolves — `HERDR_BIN_PATH` when set (a
//! path must exist), else `herdr` found on `PATH` (the same resolution
//! `crate::herdr::herdr_bin` names). Resolution happens once, in the serve
//! path (`ClauthServer::with_herdr_pane`); a server built without it is a
//! silent no-op.
//!
//! Ceiling: the counter is process-local, so a server that dies mid-delegate
//! never reports idle (herdr reclaims the state when the pane's agent process
//! exits). Two clauth servers sharing one pane have the same hole from the
//! other side: herdr keys its high-water on `--source`, both spell `clauth`,
//! and an epoch-ms seq is comparable across processes, so one session's `idle`
//! can outrank the other's live `working` and clear the icon under it. Only two
//! independent Claude Code sessions in one pane reach that; a delegate's own
//! `claude` cannot, since the depth guard refuses it a second `delegate`.
//!
//! And on herdr 0.8.0 the report only moves an icon where herdr's own Claude
//! Code integration has not already anchored that pane to a session:
//! `set_hook_authority_at` drops a report on an owner conflict unless the
//! foreground-takeover path recognizes the source, and `agent_resume::plan`
//! recognizes only herdr's own `herdr:*` sources, which no clauth report can
//! claim to be. That gate is the one clauth cannot pass by construction, never
//! the only one: `session_identity_only_integration`, the recent-agent-exit
//! check on `recent_agent_process_exit`, `route_full_lifecycle_hook_report`
//! answering `Ignore`, and `known_agent_label_conflicts_with_detected_agent`
//! each drop a report ahead of it, so an unclaimed pane can still swallow one.
//!
//! That limit is permanent, so this whole module is on a dead path. herdr keeps
//! one lifecycle authority per pane by design and closed the ask to loosen it
//! (`herdrdev/herdr#2824`, NOT_PLANNED), directing a hook that runs beside its
//! own integration to report METADATA instead. `pane report-metadata` was
//! measured applying on an anchored pane the same day, and it carries a
//! `--ttl-ms` that would also retire the mid-delegate-death ceiling above.
//! Until then the icon never moves on a pane an operator actually works in.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::logline::logline;

/// How long one report may block its caller on a hung herdr before the child
/// is killed and the report dropped. Bounds the worst-case delay a stuck herdr
/// adds to a delegate (two reports per run).
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// Process-local delegate tracking for one herdr pane. Cheap to clone: every
/// handle shares the same counters.
#[derive(Clone)]
pub(crate) struct PaneReporter {
    shared: Arc<Shared>,
}

struct Shared {
    bin: PathBuf,
    pane_id: String,
    gate: Mutex<Gate>,
}

/// The in-flight count and the seq clock under ONE lock: herdr keeps a
/// per-source high-water seq and drops anything not newer, so two reports whose
/// seqs order against their transitions leave the pane holding the loser's
/// state. Deciding and minting apart cannot give that ordering, whatever each
/// half is individually atomic over.
#[derive(Default)]
struct Gate {
    /// In-flight delegates (sync and background). Reports fire on the 0→1
    /// (`working`) and →0 (`idle`) transitions only.
    in_flight: u64,
    last_seq: u64,
}

impl Gate {
    /// Epoch-ms, forced past the last mint. herdr's high-water survives this
    /// process, so a restart's first report has to beat what the previous
    /// process left behind; a same-millisecond pair still has to separate.
    fn mint(&mut self) -> u64 {
        let seq = crate::usage::now_ms().max(self.last_seq.saturating_add(1));
        self.last_seq = seq;
        seq
    }
}

impl PaneReporter {
    /// `Some` only when the pane env is present and the herdr binary resolves.
    pub(crate) fn resolve() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID").ok()?;
        if pane_id.trim().is_empty() {
            return None;
        }
        let bin = resolve_bin()?;
        Some(Self {
            shared: Arc::new(Shared {
                bin,
                pane_id,
                gate: Mutex::new(Gate::default()),
            }),
        })
    }

    /// One in-flight delegate began: report `working` on the 0→1 transition.
    pub(crate) fn begin(&self) {
        if let Some(seq) = self.enter() {
            self.report("working", seq);
        }
    }

    /// One in-flight delegate ended: report `idle` on the →0 transition.
    fn end(&self) {
        if let Some(seq) = self.leave() {
            self.report("idle", seq);
        }
    }

    /// Count one delegate in, minting its seq under the same lock that decided
    /// to report. `None` when something else is already in flight.
    fn enter(&self) -> Option<u64> {
        let mut gate = self.gate();
        gate.in_flight = gate.in_flight.saturating_add(1);
        (gate.in_flight == 1).then(|| gate.mint())
    }

    /// Count one delegate out, minting its seq under the same lock. `None`
    /// when work remains in flight.
    fn leave(&self) -> Option<u64> {
        let mut gate = self.gate();
        debug_assert!(
            gate.in_flight > 0,
            "herdr pane reporter: end with no matching begin"
        );
        // Checked, not wrapping: an unpaired end would otherwise leave a count
        // no later `idle` can ever reach.
        let rest = gate.in_flight.checked_sub(1)?;
        gate.in_flight = rest;
        (rest == 0).then(|| gate.mint())
    }

    /// The lock is held for the count and the mint, never across the herdr
    /// spawn or its wait. A poisoned gate keeps reporting: only the debug
    /// assert above can panic under it, and the counts it protects are cosmetic.
    fn gate(&self) -> MutexGuard<'_, Gate> {
        self.shared
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The transitions without their reports. Every report costs a subprocess
    /// spawn, which is orders of magnitude wider than the window a seq minted
    /// off the decision would invert in, so a fixture that reports cannot
    /// observe the ordering these two promise.
    ///
    /// `unix` tracks the gate on the only suite that calls them: its shim herdr
    /// is POSIX shell, so the whole module compiles out on the Windows leg and
    /// an ungated helper reds that leg alone under `-D warnings`.
    #[cfg(all(test, unix))]
    pub(super) fn enter_for_test(&self) -> Option<u64> {
        self.enter()
    }

    #[cfg(all(test, unix))]
    pub(super) fn leave_for_test(&self) -> Option<u64> {
        self.leave()
    }

    /// One report to the pane: spawn `herdr pane report-agent` and wait up to
    /// [`REPORT_TIMEOUT`]. Every failure is swallowed — the pane icon is
    /// cosmetic, so a broken herdr must cost the delegate nothing.
    fn report(&self, state: &str, seq: u64) {
        // Pane id FIRST: herdr's hand-rolled parser reads it as args[0] and
        // answers `unknown option` (exit 2) to anything else in that slot.
        let mut cmd = Command::new(&self.shared.bin);
        cmd.args(["pane", "report-agent"])
            .arg(&self.shared.pane_id)
            .args(["--source", "clauth", "--agent", "claude", "--state", state])
            .arg("--seq")
            .arg(seq.to_string())
            // Never leak herdr's output into the MCP channel or the console.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            logline!(
                "clauth: herdr pane report-agent spawn failed (pane state {state} not reported)"
            );
            return;
        };
        let deadline = Instant::now() + REPORT_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        logline!(
                            "clauth: herdr pane report-agent exited {status} (pane state {state} not reported)"
                        );
                    }
                    return;
                }
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    logline!(
                        "clauth: herdr pane report-agent timed out (killed; pane state {state} not reported)"
                    );
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                // `Child`'s drop detaches without waiting, so a failed
                // `waitpid` (ECHILD, or EINTR on some libc paths) would leave
                // a zombie for the life of the server. Reap here as every
                // other arm does.
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

/// The herdr binary to report through, resolved once at server construction:
/// `HERDR_BIN_PATH` when set (a path must exist), else `herdr` found on
/// `PATH`.
fn resolve_bin() -> Option<PathBuf> {
    let raw = crate::herdr::herdr_bin();
    let candidate = Path::new(&raw);
    // Path-like (absolute or carrying a separator): must exist as a file.
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    // Bare name: first executable hit on PATH (exec bit on Unix, the usual
    // extensions on Windows).
    crate::plugin_probe::on_path(&raw)
}

/// RAII in-flight tracker half: the drop reports `idle` once nothing else is in
/// flight, on every exit path, panic included.
pub(crate) struct InFlightGuard {
    reporter: PaneReporter,
}

impl InFlightGuard {
    /// Track only — every delegate's `begin` runs at commit-to-launch, and the
    /// guard is created first thing in the run's own task so no early return
    /// can skip the decrement and so the panel follows the RUN rather than the
    /// call that started it.
    pub(crate) fn end_only(reporter: PaneReporter) -> Self {
        Self { reporter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.reporter.end();
    }
}
