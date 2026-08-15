#![allow(clippy::unwrap_used, clippy::expect_used)]

//! F1 (filed 2026-08-14): a detached background delegate still running when
//! `HomeSandbox` tears down must never touch the operator's REAL `$HOME`.
//! `launch_background_delegate` detaches via `tokio::task::spawn_blocking`
//! with no handle the caller keeps, so nothing joined it before
//! `HOME_OVERRIDE` cleared. `HomeSandbox::drop` now blocks on
//! `testutil::register_background_task`'s completion signal for the same
//! reason it already joins `tui::TEST_WORKERS`.

use super::*;
use crate::testutil::HomeSandbox;
use std::time::Duration;

/// Drive one detached background delegate directly — skipping `delegate()`'s
/// own pre-flight, which isn't part of the race under test — against a
/// profile name that exists in neither the sandboxed nor the real config.
/// `run_delegate` fails fast at `load_config().find` regardless of which
/// `$HOME` it resolves; only the WRITE LOCATION of the resulting `done` job
/// file differs between the two, which is exactly the leak this test is
/// pinned on.
#[test]
fn detached_task_still_running_at_teardown_never_touches_the_real_home() {
    let profile = format!("clauth-f1-leak-probe-{}", std::process::id());
    let home = HomeSandbox::new();

    // Arm the gate only after `home` holds `HOME_TEST_LOCK`: a single global
    // slot shared by every test, so arming it earlier could gate some other
    // test's unrelated background task instead of this one.
    let release_gate = arm_detach_gate();

    let spec = reserve_background_job(&profile, None, None, true).expect("reserve background job");
    let job_id = spec.job_id.clone();
    // `spawn_blocking` needs an entered Tokio runtime; the runtime itself must
    // outlive the spawn (dropping it can wait on outstanding blocking tasks,
    // which would fold its own join into this test's timing), so it's kept
    // alive for the rest of the function rather than dropped right after.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        launch_background_delegate(
            profile.clone(),
            BackgroundOpts {
                prompt: std::sync::Arc::from("hello"),
                model: None,
                cwd: None,
                env: HashMap::new(),
                extra_args: Vec::new(),
                timeout_secs: None,
                idle_secs: None,
                resume: None,
                isolation: Isolation::Isolated,
                depth: 0,
            },
            spec,
            None,
        );
    });

    // Release the gate from a second thread so it can race `drop(home)`:
    // pre-fix, `drop` returns without waiting on the task at all, so the
    // release only needs to land some time after `drop` was CALLED, not
    // after it returned — a short sleep covers that. Post-fix, `drop` blocks
    // on the task's completion signal until this fires, and `HOME_TEST_LOCK`
    // stays held for that whole wait (`HomeSandbox`'s custom `drop` body runs
    // before its own `_guard` field drops), so no other test can race the
    // override in either branch.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let _ = release_gate.send(());
    });
    drop(home);
    releaser.join().expect("releaser thread joins");

    // The real, OS-resolved home — bypassing any test override, which may
    // legitimately belong to a different, unrelated concurrent test by now.
    let real_home = dirs::home_dir().expect("resolve real home for the probe");
    let real_job_path = real_home
        .join(".clauth")
        .join("jobs")
        .join(format!("{job_id}.json"));

    // The write is one small local JSON file with no network hop; poll
    // briefly rather than asserting the instant `drop` returns.
    let mut leaked = false;
    for _ in 0..25 {
        if real_job_path.exists() {
            leaked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Never delete anything but this test's own uniquely-named job file — the
    // real jobs dir is the operator's live directory.
    if leaked {
        let _ = std::fs::remove_file(&real_job_path);
    }

    assert!(
        !leaked,
        "detached background delegate wrote its job file into the REAL jobs \
         dir at {} — HomeSandbox::drop returned (or the task ran) before the \
         detached task finished, so it resolved the operator's real $HOME \
         instead of the sandbox",
        real_job_path.display(),
    );
}
