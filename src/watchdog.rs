//! Filesystem-event-driven reconcile for credential and config files.
//!
//! Wraps `notify` to watch individual files with `RecursiveMode::NonRecursive`.
//! Events are debounced to a single wake signal per burst, with a fallback
//! ticker so a lost event never stalls reconcile permanently.

use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, bounded, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::logline::logline;

/// Debounce coalescing window — writes often produce multiple events (CREATE +
/// MODIFY + CLOSE_WRITE) and reconciling on each one is wasted work.
pub(crate) const DEBOUNCE_MS: Duration = Duration::from_millis(200);

/// Fallback poll interval. When no event arrives for this long, run a full
/// reconcile as a safety net against lost or silently-rejected events.
pub(crate) const FALLBACK_INTERVAL: Duration = Duration::from_secs(30);

/// After the watchdog's own [`tick()`] writes a file, events from that
/// write are skipped for this long to prevent a self-trigger loop.
pub(crate) const WRITE_COOLDOWN: Duration = Duration::from_millis(500);

/// A running filesystem watcher.
pub(crate) struct EventWatcher {
    /// Held to keep the watcher alive. Dropped on watchdog exit.
    #[allow(dead_code)]
    handle: RecommendedWatcher,
    /// Debounced wake signals from the coalescer thread. Disconnects when the
    /// debouncer thread exits (panic or early return) — the watchdog detects
    /// this and falls back to polling.
    pub(crate) wake: Receiver<()>,
    /// Debouncer thread — joined on drop so a panic is observed rather than
    /// silently disconnecting `wake`.
    #[allow(dead_code)]
    _debouncer: std::thread::JoinHandle<()>,
}

/// Try to create a filesystem watcher for `paths`.
///
/// Watches individual files (never recursively) so the inotify surface area
/// stays bounded. Returns `None` when:
/// - `notify::recommended_watcher` returns an error (inotify instance limit,
///   unsupported platform).
/// - Any `path` cannot be added to the watch list.
///
/// On failure the reason is logged; the caller falls back to polling.
pub(crate) fn try_start(paths: &[PathBuf]) -> Option<EventWatcher> {
    let (raw_tx, raw_rx) = unbounded();

    let mut handle = match notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            if res.is_ok() {
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

    for path in paths {
        if let Err(e) = handle.watch(path, RecursiveMode::NonRecursive) {
            logline!("clauth: fs watcher cannot watch {}: {e}", path.display());
            return None;
        }
    }

    let (wake_tx, wake_rx) = bounded::<()>(1);

    // Debouncer thread: coalesces events within DEBOUNCE_MS.
    let debouncer = std::thread::Builder::new()
        .name("clauth-wdog-evt".into())
        .spawn(move || {
            loop {
                // Block until the first event or the watcher is dropped
                // (disconnects raw_rx).
                if raw_rx.recv().is_err() {
                    return;
                }
                let _ = wake_tx.send(());
                // Drain remaining events within the debounce window so the
                // watchdog reconciles once per burst.
                loop {
                    match raw_rx.recv_timeout(DEBOUNCE_MS) {
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

/// Build the set of file paths the watchdog watches.
///
/// These are the files whose changes trigger credential or config reconcile,
/// not directories — `notify` on individual files with `NonRecursive` is what
/// keeps the inotify surface small.
pub(crate) fn watch_paths(
    runtime: &std::path::Path,
    canonical_creds: &std::path::Path,
    claude_home: &std::path::Path,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(4);

    // Runtime credentials: CC may rewrite this file during a re-login (Real
    // mode: unlink + create; Fake mode: in-place edit through the mirror).
    paths.push(runtime.join(".credentials.json"));

    // Canonical / profile credentials: clauth-side writes (rotation, switch,
    // login) land here.
    paths.push(canonical_creds.to_path_buf());

    // Global `.claude.json`: CC rewrites this constantly.
    if let Some(home) = claude_home.parent() {
        paths.push(home.join(".claude.json"));
    } else {
        // Degenerate case — claude_home is the root. The reconciler looks up
        // `~/.claude.json` through the profile module's `home_dir()`, so this
        // path is a best-effort approximation; the fallback ticker catches
        // whatever this misses.
        paths.push(std::path::PathBuf::from(".claude.json"));
    }

    // Global `settings.json`.
    paths.push(claude_home.join("settings.json"));

    paths
}
