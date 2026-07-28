//! Filesystem-event-driven reconcile for credential and config files, with a
//! polling fallback for when events are unavailable or the watcher dies.
//!
//! Watches the parent DIRECTORY of every interesting file rather than the file
//! itself. Every one of those files is published by `rename(2)` (`copy_file`,
//! `atomic_write_600`), which unlinks the watched inode: inotify then drops the
//! watch (`IN_DELETE_SELF` / `IN_IGNORED`) with nothing re-arming it, so a file
//! watch survives exactly one write. A directory inode outlives its children's
//! renames.
//!
//! Dir watches widen the event surface, so [`Interest`] narrows each watch back
//! down to the children that can actually change a reconcile's outcome.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::logline::logline;

/// The three legs one watchdog iteration can run. Split because the polling
/// fallback runs the config leg ten times per credential leg, where the event
/// path runs all three together.
pub(crate) trait Reconcile {
    /// Cross-profile `.claude.json` + `settings.json` sync.
    fn config(&self);
    /// Credential reconcile between the runtime link and the profile store.
    fn credentials(&self);
    /// Pick up a daemon-requested member swap.
    fn swap_poll(&self);
}

/// Every interval the watchdog loop runs on. A struct rather than consts so a
/// test drives the loop on a bounded wait of milliseconds instead of minutes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timings {
    /// Coalescing window. One write produces several events (CREATE + MODIFY +
    /// CLOSE_WRITE, or MOVED_FROM + MOVED_TO); reconciling per event is waste.
    pub(crate) debounce: Duration,
    /// Minimum spacing between reconciles, measured from the END of the last
    /// one. Every reconcile writes into a watched directory, so without it a
    /// reconcile re-triggers on its own publishes.
    pub(crate) cooldown: Duration,
    /// Safety net for an event the watcher never delivered.
    pub(crate) fallback: Duration,
    /// Polling-fallback config cadence. Tighter than the credential leg because
    /// Claude Code rewrites `.claude.json` constantly; 100 ms keeps the window
    /// in which one profile observes another's stale shared state small. Also
    /// bounds watchdog-thread shutdown latency to one tick.
    pub(crate) config_poll: Duration,
    /// Polling-fallback credential cadence. 1 s instead of longer because
    /// fake-symlink mode needs a tight upper bound on how long a session can
    /// read stale credentials after a sibling refreshes — every additional
    /// second is another window in which a 401 could revoke an already-rotated
    /// refresh token chain. The event loop reuses it for the swap-poll ticker:
    /// the daemon's intent lands in `~/.clauth/live_sessions/`, which no watch
    /// covers, so that leg has no filesystem signal to key on.
    pub(crate) credential_poll: Duration,
}

/// What `clauth start` runs on.
pub(crate) const PRODUCTION: Timings = Timings {
    debounce: Duration::from_millis(200),
    cooldown: Duration::from_millis(500),
    fallback: Duration::from_secs(30),
    config_poll: Duration::from_millis(100),
    credential_poll: Duration::from_secs(1),
};

/// Which children of a watched directory are worth a reconcile.
#[derive(Debug, Clone)]
pub(crate) enum Interest {
    /// Only these names. Used where the directory holds unrelated hot state.
    Names(Vec<OsString>),
    /// Every child except clauth's own staging files — the tree mirror's
    /// surface, where the set of interesting names is the tree itself.
    AnyChild,
}

/// One watched directory plus the children that matter inside it.
#[derive(Debug, Clone)]
pub(crate) struct WatchSpec {
    dir: PathBuf,
    interest: Interest,
}

impl WatchSpec {
    pub(crate) fn new(dir: impl Into<PathBuf>, interest: Interest) -> Self {
        Self {
            dir: dir.into(),
            interest,
        }
    }
}

/// A running filesystem watcher.
pub(crate) struct EventWatcher {
    /// Held to keep the watcher alive. Dropped on watchdog exit.
    #[allow(dead_code)]
    handle: RecommendedWatcher,
    /// Debounced wake signals from the coalescer thread. Disconnects when the
    /// debouncer thread exits (panic or early return) — the watchdog detects
    /// this and falls back to polling.
    pub(crate) wake: Receiver<()>,
    /// Kept so the debouncer outlives the watcher rather than being detached.
    #[allow(dead_code)]
    _debouncer: std::thread::JoinHandle<()>,
}

/// clauth publishes every file as a hidden `.<name>.tmp.<pid>[.<seq>]` sibling
/// renamed into place (`profile::tmp_sibling`, `relink_to_canonical`). Waking on
/// the staging half costs a reconcile per publish and can only ever observe a
/// path that is about to move anyway.
///
/// Also the fake-mode tree mirror's skip rule (`runtime::union_children`): a
/// walk that treats one as tree content either fails when the source is renamed
/// away mid-copy, or lands an orphan the mirror can never delete.
pub(crate) fn is_staging(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|n| n.starts_with('.') && n.contains(".tmp."))
}

/// Whether a changed `path` can alter a reconcile's outcome. Pure, so the filter
/// that bounds the event surface is pinned without a filesystem.
fn wants(specs: &[WatchSpec], path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    specs.iter().any(|spec| {
        spec.dir == dir
            && match &spec.interest {
                Interest::Names(names) => names.iter().any(|n| n.as_os_str() == name),
                Interest::AnyChild => !is_staging(name),
            }
    })
}

/// The directories the watchdog watches, and what it cares about in each.
///
/// The runtime tree and `~/.claude/` take every child: fake-symlink mode mirrors
/// them against each other, so any entry appearing on one side is reconcile
/// input. The profile store and `$HOME` take a name list — the first holds the
/// per-profile JSON caches a scheduler rewrites on its own cadence, the second
/// is the operator's whole home directory and only `.claude.json` is ours.
///
/// Ceiling: NonRecursive, so a change nested under `~/.claude/projects/` or the
/// runtime tree reaches reconcile on the fallback interval rather than on its
/// event. Recursive would cost one inotify watch per project directory and turn
/// every Claude Code transcript append into an event. Upgrade path if deep
/// fake-mode latency ever matters: watch `<tree>/projects` explicitly rather
/// than making the whole tree recursive.
pub(crate) fn watch_specs(
    runtime: &Path,
    canonical_creds: &Path,
    claude_home: &Path,
) -> Vec<WatchSpec> {
    let mut specs = Vec::with_capacity(4);

    // Runtime tree: `.credentials.json` (Claude Code rewrites it on a re-login),
    // `settings.json`, and every entry the fake-mode mirror carries.
    specs.push(WatchSpec::new(runtime, Interest::AnyChild));

    // The profile's credential store. A swap moves `canonical_creds` to another
    // member's directory and this list is not rebuilt, which costs nothing: a
    // swap only happens under `LinkMode::Real`, where the runtime path is a
    // symlink onto the store and a store-side write needs no reconcile to be
    // visible. Fake mode, where the mirror does need it, never swaps.
    if let (Some(dir), Some(name)) = (canonical_creds.parent(), canonical_creds.file_name()) {
        specs.push(WatchSpec::new(dir, Interest::Names(vec![name.to_owned()])));
    }

    // Global `.claude.json`, a sibling of `~/.claude/` rather than a child.
    if let Some(home) = claude_home.parent() {
        specs.push(WatchSpec::new(
            home,
            Interest::Names(vec![OsString::from(".claude.json")]),
        ));
    }

    // The operator's `~/.claude/`: `settings.json` plus the mirror's other side.
    specs.push(WatchSpec::new(claude_home, Interest::AnyChild));

    specs
}

/// Try to create a filesystem watcher for `specs`.
///
/// A directory that cannot be armed is logged and skipped rather than failing
/// the whole watcher — one absent `~/.claude/` would otherwise put every session
/// on the fallback interval. Returns `None` only when nothing could be armed, or
/// when `notify` itself is unavailable (inotify instance limit, unsupported
/// platform); the caller then falls back to polling.
pub(crate) fn try_start(specs: &[WatchSpec], debounce: Duration) -> Option<EventWatcher> {
    let (raw_tx, raw_rx) = unbounded();

    let filter: Vec<WatchSpec> = specs.to_vec();
    let mut handle = match notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            let Ok(event) = res else { return };
            // A dropped-event overflow says the queue lost changes nobody can
            // name, so it must reconcile even though it carries no path.
            if event.need_rescan() || event.paths.iter().any(|p| wants(&filter, p)) {
                let _ = raw_tx.send(());
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            logline!("clauth: fs watcher unavailable: {e}");
            return None;
        }
    };

    let mut armed = 0usize;
    for spec in specs {
        match handle.watch(&spec.dir, RecursiveMode::NonRecursive) {
            Ok(()) => armed += 1,
            Err(e) => logline!(
                "clauth: fs watcher cannot watch {}: {e}",
                spec.dir.display()
            ),
        }
    }
    if armed == 0 {
        return None;
    }

    let (wake_tx, wake_rx) = bounded::<()>(1);

    // Debouncer thread: coalesces a burst of events into one wake.
    let debouncer = std::thread::Builder::new()
        .name("clauth-wdog-evt".into())
        .spawn(move || {
            loop {
                // Block until the first event or the watcher is dropped
                // (disconnects raw_rx).
                if raw_rx.recv().is_err() {
                    return;
                }
                // `try_send` on a `bounded(1)`: a wake already queued covers
                // this event too, and blocking here would stall the drain
                // below behind a reconcile.
                match wake_tx.try_send(()) {
                    Ok(()) | Err(crossbeam_channel::TrySendError::Full(())) => {}
                    Err(crossbeam_channel::TrySendError::Disconnected(())) => return,
                }
                // Coalesce for a FIXED window rather than until events go idle.
                // A sustained write stream never goes idle, so an idle-gap
                // window emits one wake at the head of the burst and then
                // nothing at all until the stream stops — every change in
                // between reaching reconcile only on the fallback interval.
                let window_ends = Instant::now() + debounce;
                loop {
                    let left = window_ends.saturating_duration_since(Instant::now());
                    match raw_rx.recv_timeout(left) {
                        Ok(()) => {} // coalesce
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        })
        .ok()?;

    Some(EventWatcher {
        handle,
        wake: wake_rx,
        _debouncer: debouncer,
    })
}

/// Why the event loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    /// The shutdown channel fired or disconnected.
    Shutdown,
    /// The debouncer thread died; the caller must fall back to polling.
    WatcherLost,
}

/// Run the watchdog until `shutdown` fires: event-driven while a watcher can be
/// armed, polling otherwise.
pub(crate) fn run(specs: &[WatchSpec], shutdown: &Receiver<()>, t: &Timings, r: &dyn Reconcile) {
    if let Some(watcher) = try_start(specs, t.debounce) {
        match run_events(&watcher.wake, shutdown, t, r) {
            Exit::Shutdown => return,
            Exit::WatcherLost => {
                logline!("clauth: fs watcher event channel disconnected, switching to poll")
            }
        }
    }
    run_poll(shutdown, t, r);
}

/// Event-driven loop. Reconciles on a wake, no faster than one `cooldown` after
/// the previous reconcile RETURNED, with the fallback ticker covering an event
/// that never arrived.
pub(crate) fn run_events(
    wake: &Receiver<()>,
    shutdown: &Receiver<()>,
    t: &Timings,
    r: &dyn Reconcile,
) -> Exit {
    let fallback = crossbeam_channel::tick(t.fallback);
    let swap_poll = crossbeam_channel::tick(t.credential_poll);
    // `None` rather than a back-dated `Instant`: the first wake must reconcile
    // at once, and `Instant` arithmetic can underflow near boot.
    let mut last_reconcile: Option<Instant> = None;
    // A wake inside the cooldown is DEFERRED, never dropped: `wake` is
    // `bounded(1)` and the debouncer discards what it coalesces, so a dropped
    // one has no replay and its change waits out the whole fallback interval.
    let mut pending = false;

    loop {
        let idle = if pending {
            last_reconcile.map_or(Duration::ZERO, |at| t.cooldown.saturating_sub(at.elapsed()))
        } else {
            t.fallback
        };
        crossbeam_channel::select! {
            recv(shutdown) -> _ => return Exit::Shutdown,
            recv(swap_poll) -> _ => r.swap_poll(),
            recv(wake) -> res => {
                if res.is_err() {
                    return Exit::WatcherLost;
                }
                pending = true;
            }
            recv(fallback) -> _ => pending = true,
            // Only reachable with a deferred wake outstanding; `idle` is then
            // exactly what is left of its cooldown.
            default(idle) => {}
        }
        if pending && last_reconcile.is_none_or(|at| at.elapsed() >= t.cooldown) {
            pending = false;
            r.config();
            r.credentials();
            r.swap_poll();
            last_reconcile = Some(Instant::now());
        }
    }
}

/// Polling fallback, reached when no directory could be armed or the debouncer
/// died. Config reconcile every `config_poll`, credentials every
/// `credential_poll`.
pub(crate) fn run_poll(shutdown: &Receiver<()>, t: &Timings, r: &dyn Reconcile) {
    let cred_every = (t.credential_poll.as_millis() / t.config_poll.as_millis().max(1)).max(1);
    let mut until_cred = cred_every;
    let ticker = crossbeam_channel::tick(t.config_poll);
    loop {
        crossbeam_channel::select! {
            recv(shutdown) -> _ => return,
            recv(ticker) -> _ => {
                r.config();
                until_cred -= 1;
                if until_cred == 0 {
                    until_cred = cred_every;
                    r.credentials();
                    r.swap_poll();
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/watchdog.rs"]
mod tests;
