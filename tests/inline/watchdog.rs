//! Inline tests for `crate::watchdog` — the event filter, the watch's survival
//! of the rename every clauth write publishes through, and the two loop
//! properties (cooldown measured from the reconcile's END, a cooled-down wake
//! deferred rather than dropped).
//!
//! The loop tests drive `run_events` through a plain channel instead of a real
//! watcher: the loop's timing behavior is what they pin, and a filesystem in the
//! path would only add flake. `a_dir_watch_survives_...` is the one that needs
//! real inotify, and every wait in this file is bounded.

use super::*;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_channel::Sender;

/// Long enough that a fallback tick cannot be mistaken for an event-driven or
/// deferred reconcile inside any of these tests.
const NEVER: Duration = Duration::from_secs(600);

/// Every wait here is bounded by this rather than blocking, so a regression
/// fails the suite instead of hanging it.
const BOUND: Duration = Duration::from_secs(5);

fn timings(cooldown: Duration) -> Timings {
    Timings {
        debounce: Duration::from_millis(20),
        cooldown,
        fallback: NEVER,
        config_poll: NEVER,
        credential_poll: NEVER,
    }
}

/// Records one span per credential leg and signals it, so a test can await each
/// reconcile on a deadline. `work` simulates a reconcile slower than its own
/// cooldown — the fake-mode tree walk plus a state flock that can block for 25 s.
struct Recorder {
    spans: Mutex<Vec<(Instant, Instant)>>,
    configs: AtomicUsize,
    swap_polls: AtomicUsize,
    work: Duration,
    done: Sender<()>,
}

impl Recorder {
    fn new(work: Duration, done: Sender<()>) -> Self {
        Self {
            spans: Mutex::new(Vec::new()),
            configs: AtomicUsize::new(0),
            swap_polls: AtomicUsize::new(0),
            work,
            done,
        }
    }

    fn span(&self, i: usize) -> (Instant, Instant) {
        self.spans.lock().unwrap_or_else(|p| p.into_inner())[i]
    }
}

impl Reconcile for Recorder {
    fn config(&self) {
        self.configs.fetch_add(1, Ordering::Relaxed);
    }
    fn credentials(&self) {
        let entered = Instant::now();
        std::thread::sleep(self.work);
        self.spans
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((entered, Instant::now()));
        let _ = self.done.send(());
    }
    fn swap_poll(&self) {
        self.swap_polls.fetch_add(1, Ordering::Relaxed);
    }
}

/// Exactly how `copy_file` and `atomic_write_600` land a file: write a hidden
/// staging sibling, then rename it over the target.
fn publish(dst: &Path, bytes: &[u8]) {
    let staging = crate::profile::tmp_sibling(dst);
    std::fs::write(&staging, bytes).expect("write staging");
    std::fs::rename(&staging, dst).expect("publish");
}

/// The defect that made the event path permanently self-disabling: a watch on
/// the FILE arms `IN_DELETE_SELF`/`IN_MOVE_SELF`, and the rename that publishes
/// every clauth-written file unlinks that inode, so notify drops the watch with
/// nothing re-arming it. One write per path and the watcher is dead — silently,
/// because the channel stays connected. A directory inode outlives its
/// children's renames.
#[test]
fn a_dir_watch_survives_the_rename_that_publishes_a_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("settings.json");
    std::fs::write(&target, b"{}").expect("seed target");

    let debounce = Duration::from_millis(50);
    let specs = vec![WatchSpec::new(
        tmp.path(),
        Interest::Names(vec!["settings.json".into()]),
    )];
    let watcher = try_start(&specs, debounce).expect("watcher");

    for round in 0..3u32 {
        // Clear of the previous burst's coalescing window, so a missing wake is
        // a dead watch rather than a swallowed duplicate.
        std::thread::sleep(debounce * 3);
        publish(&target, format!(r#"{{"round":{round}}}"#).as_bytes());
        assert!(
            watcher.wake.recv_timeout(BOUND).is_ok(),
            "publish {round} produced no wake: the watch did not survive a rename"
        );
    }
}

/// A burst coalesces into one wake per WINDOW, never into one wake for the whole
/// burst. A write stream that keeps the queue non-empty never goes idle, so an
/// idle-gap window delivers the head event and then nothing until the stream
/// stops — every change in between reaching reconcile only on the fallback.
#[test]
fn a_sustained_write_stream_keeps_waking_the_watchdog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("settings.json");
    std::fs::write(&target, b"{}").expect("seed target");

    let debounce = Duration::from_millis(30);
    let specs = vec![WatchSpec::new(tmp.path(), Interest::AnyChild)];
    let watcher = try_start(&specs, debounce).expect("watcher");

    let deadline = Instant::now() + debounce * 20;
    let mut publishes = 0u32;
    let mut wakes = 0u32;
    while Instant::now() < deadline {
        publishes += 1;
        publish(&target, format!(r#"{{"n":{publishes}}}"#).as_bytes());
        std::thread::sleep(debounce / 6);
        if watcher.wake.try_recv().is_ok() {
            wakes += 1;
        }
    }
    assert!(
        wakes >= 3,
        "{publishes} publishes with no idle gap produced {wakes} wakes: \
         the burst was swallowed rather than coalesced"
    );
}

/// The measured "reflected without waiting a tick" claim. Every ticker is set
/// past the bound, so a publish into a watched directory is the only thing that
/// can drive a reconcile at all — which is what makes a green here mean the
/// event path and not a poll that happened to land.
#[test]
fn a_store_publish_reconciles_with_every_ticker_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = tmp.path().join("store").join("credentials.json");
    let runtime = tmp.path().join("runtime-1-0");
    let claude_home = tmp.path().join(".claude");
    for dir in [
        store.parent().expect("store parent"),
        runtime.as_path(),
        claude_home.as_path(),
    ] {
        std::fs::create_dir_all(dir).expect("mkdir");
    }
    std::fs::write(&store, b"{}").expect("seed store");

    let specs = watch_specs(&runtime, &store, &claude_home);
    let t = timings(Duration::ZERO);
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = crossbeam_channel::unbounded();
    let rec = Recorder::new(Duration::ZERO, done_tx);

    std::thread::scope(|scope| {
        scope.spawn(|| run(&specs, &shutdown_rx, &t, &rec));
        // `run` arms the watcher on its own thread; a publish that beats it
        // would leave nothing to observe.
        std::thread::sleep(t.debounce * 5);

        let started = Instant::now();
        publish(&store, br#"{"claudeAiOauth":{"accessToken":"fresh"}}"#);
        done_rx
            .recv_timeout(BOUND)
            .expect("a publish into the credential store drove no reconcile");
        let took = started.elapsed();
        assert!(
            took < PRODUCTION.credential_poll,
            "the event path took {took:?}, no better than the {:?} credential \
             poll it replaced",
            PRODUCTION.credential_poll
        );

        drop(shutdown_tx);
    });
}

/// The filter is what keeps a directory watch from costing a reconcile per
/// unrelated write in a hot directory — and what keeps clauth's own staging
/// halves from waking the loop on every publish it makes.
#[test]
fn the_filter_takes_named_children_and_drops_staging_siblings() {
    let store = Path::new("/clauth/profiles/acct");
    let tree = Path::new("/clauth/profiles/acct/runtime-1-0");
    let specs = vec![
        WatchSpec::new(store, Interest::Names(vec!["credentials.json".into()])),
        WatchSpec::new(tree, Interest::AnyChild),
    ];

    assert!(wants(&specs, &store.join("credentials.json")));
    assert!(
        !wants(&specs, &store.join("kick_block.json")),
        "an unnamed sibling in the store must not wake the watchdog"
    );
    assert!(
        !wants(&specs, &store.join("sub").join("credentials.json")),
        "the watch is NonRecursive, so a nested path is not this directory's child"
    );

    assert!(wants(&specs, &tree.join("statusline.sh")));
    assert!(wants(&specs, &tree.join(".credentials.json")));
    assert!(
        !wants(
            &specs,
            &crate::profile::tmp_sibling(&tree.join("settings.json"))
        ),
        "`tmp_sibling`'s staging half is our own write in flight"
    );
    assert!(
        // `runtime::relink_to_canonical`'s staging name, which carries no seq.
        !wants(&specs, &tree.join(".credentials.json.tmp.4242")),
        "the relink staging half is our own write in flight"
    );
    assert!(!wants(&specs, Path::new("/elsewhere/credentials.json")));
}

/// `wake` is `bounded(1)` and the debouncer discards what it coalesces, so an
/// event dropped for being inside the cooldown has no replay: its change waits
/// out the entire fallback interval. It must be deferred and serviced once the
/// cooldown expires.
#[test]
fn a_wake_inside_the_cooldown_is_deferred_not_dropped() {
    let cooldown = Duration::from_millis(300);
    let t = timings(cooldown);
    let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(Duration::ZERO, done_tx);

    std::thread::scope(|scope| {
        scope.spawn(|| run_events(&wake_rx, &shutdown_rx, &t, &rec));

        wake_tx.send(()).expect("first wake");
        done_rx.recv_timeout(BOUND).expect("first reconcile");
        // Lands well inside the cooldown the reconcile just started.
        wake_tx.send(()).expect("second wake");
        done_rx
            .recv_timeout(BOUND)
            .expect("the cooled-down wake was dropped instead of deferred");

        drop(shutdown_tx);
    });

    assert_eq!(
        rec.configs.load(Ordering::Relaxed),
        2,
        "both reconciles must run every leg"
    );
}

/// Stamping the cooldown before the reconcile spends it on the reconcile
/// itself: anything slower than the cooldown (a fake-mode tree walk, a
/// `with_state_lock` that can block for 25 s) returns already cooled down and
/// re-triggers on its own writes.
#[test]
fn the_cooldown_is_measured_from_the_end_of_the_reconcile() {
    let cooldown = Duration::from_millis(200);
    let work = cooldown * 2;
    let t = timings(cooldown);
    let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(work, done_tx);

    std::thread::scope(|scope| {
        scope.spawn(|| run_events(&wake_rx, &shutdown_rx, &t, &rec));

        wake_tx.send(()).expect("first wake");
        // Mid-reconcile, standing in for the events that reconcile's own writes
        // produce in the directories it publishes into.
        std::thread::sleep(work / 4);
        wake_tx.send(()).expect("second wake");

        done_rx.recv_timeout(BOUND).expect("first reconcile");
        done_rx.recv_timeout(BOUND).expect("second reconcile");

        drop(shutdown_tx);
    });

    let (_, first_returned) = rec.span(0);
    let (second_entered, _) = rec.span(1);
    let gap = second_entered.duration_since(first_returned);
    assert!(
        gap >= cooldown,
        "the second reconcile started {gap:?} after the first returned, \
         inside the {cooldown:?} cooldown: the cooldown was spent by the \
         reconcile that owned it"
    );
}

/// Events unavailable — no directory could be armed — must still reconcile, on
/// the poll cadence and at the poll ratio.
#[test]
fn the_poll_fallback_reconciles_when_no_directory_can_be_armed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let specs = vec![WatchSpec::new(
        tmp.path().join("does-not-exist"),
        Interest::AnyChild,
    )];
    let t = Timings {
        debounce: Duration::from_millis(20),
        cooldown: Duration::ZERO,
        fallback: NEVER,
        config_poll: Duration::from_millis(20),
        credential_poll: Duration::from_millis(200),
    };
    assert!(
        try_start(&specs, t.debounce).is_none(),
        "an unwatchable directory must leave the caller on the polling path"
    );

    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(Duration::ZERO, done_tx);

    std::thread::scope(|scope| {
        scope.spawn(|| run(&specs, &shutdown_rx, &t, &rec));

        done_rx
            .recv_timeout(BOUND)
            .expect("the polling fallback never reconciled credentials");
        assert_eq!(
            rec.configs.load(Ordering::Relaxed),
            10,
            "the config leg runs `credential_poll / config_poll` times per credential leg"
        );

        done_rx
            .recv_timeout(BOUND)
            .expect("the polling fallback stopped after one credential reconcile");
        assert!(rec.swap_polls.load(Ordering::Relaxed) >= 2);

        drop(shutdown_tx);
    });
}

/// `watch_specs` covers every file the reconcile reads, and covers them by
/// their PARENT so no entry is armed on an inode a rename will unlink.
#[test]
fn watch_specs_cover_each_reconciled_file_through_its_directory() {
    let runtime = Path::new("/clauth/profiles/acct/runtime-1-0");
    let store = Path::new("/clauth/profiles/acct/credentials.json");
    let claude_home = Path::new("/home/u/.claude");
    let specs = watch_specs(runtime, store, claude_home);

    for path in [
        runtime.join(".credentials.json"),
        runtime.join("settings.json"),
        store.to_path_buf(),
        Path::new("/home/u/.claude.json").to_path_buf(),
        claude_home.join("settings.json"),
        // The fake-mode mirror's own surface, which no file watch ever covered.
        claude_home.join("statusline.sh"),
        runtime.join("CLAUDE.md"),
    ] {
        assert!(wants(&specs, &path), "{} is unwatched", path.display());
    }
    assert!(
        !wants(&specs, Path::new("/home/u/.bash_history")),
        "watching $HOME for .claude.json must not wake on the rest of it"
    );
    assert!(
        !wants(
            &specs,
            &Path::new("/clauth/profiles/acct").join("kick_block.json")
        ),
        "the profile store directory holds caches a scheduler rewrites on its own cadence"
    );
}
