#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]
#![cfg(unix)]

//! Pins for the herdr pane-status reports behind `delegate`: the `herdr pane
//! report-agent` call shape, the working/idle pairing on every sync exit path,
//! the background in-flight counter, and the gating. A SHIM herdr binary (a
//! shell script pointed at by `HERDR_BIN_PATH`, or a bare name found on a
//! pinned `PATH`) appends its argv to a log file, so every assertion reads the
//! real spawned argv. Nothing here touches a live herdr socket.
//!
//! unix-only: the shim is POSIX shell, which Windows cannot execute.

use super::herdr_report::{InFlightGuard, PaneReporter};
use super::*;
use crate::profile::{AppConfig, AppState};
use crate::testutil::HomeSandbox;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// RAII pin for the two herdr env vars (`HERDR_PANE_ID`, `HERDR_BIN_PATH`),
/// restored on drop (even on panic). Borrows the [`HomeSandbox`] the way
/// `testutil::ConfigDirSandbox` does: the env is a process-global serialized by
/// `HOME_TEST_LOCK`, which the sandbox holds, so the pin must never outlive it.
struct EnvPin<'a> {
    prev_pane: Option<std::ffi::OsString>,
    prev_bin: Option<std::ffi::OsString>,
    _home: std::marker::PhantomData<&'a HomeSandbox>,
}

impl<'a> EnvPin<'a> {
    fn new(_home: &'a HomeSandbox, pane: Option<&str>, bin: Option<&Path>) -> Self {
        let prev_pane = std::env::var_os("HERDR_PANE_ID");
        let prev_bin = std::env::var_os("HERDR_BIN_PATH");
        // SAFETY: test-only; serialized by HOME_TEST_LOCK (held by the borrowed
        // sandbox); restored on drop.
        unsafe {
            match pane {
                Some(v) => std::env::set_var("HERDR_PANE_ID", v),
                None => std::env::remove_var("HERDR_PANE_ID"),
            }
            match bin {
                Some(p) => std::env::set_var("HERDR_BIN_PATH", p),
                None => std::env::remove_var("HERDR_BIN_PATH"),
            }
        }
        Self {
            prev_pane,
            prev_bin,
            _home: std::marker::PhantomData,
        }
    }
}

impl Drop for EnvPin<'_> {
    fn drop(&mut self) {
        // SAFETY: same as `new` — restore the prior value under the same lock.
        unsafe {
            match &self.prev_pane {
                Some(v) => std::env::set_var("HERDR_PANE_ID", v),
                None => std::env::remove_var("HERDR_PANE_ID"),
            }
            match &self.prev_bin {
                Some(v) => std::env::set_var("HERDR_BIN_PATH", v),
                None => std::env::remove_var("HERDR_BIN_PATH"),
            }
        }
    }
}

/// Write a POSIX shim named `name` whose body runs after the shebang, chmod
/// +x, and return its path. A report spawns it with `HERDR_BIN_PATH`; `$0`
/// resolves to the shim itself, so a log relative to it needs no embedded path.
fn write_shim(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write shim");
    let mut perms = std::fs::metadata(&path).expect("stat shim").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod shim");
    path
}

/// Shim that appends its full argv to `report.log` beside itself and exits 0.
fn echo_shim(dir: &Path, name: &str) -> PathBuf {
    write_shim(dir, name, r#"echo "$@" >> "$(dirname "$0")/report.log""#)
}

/// Same log line, then exit 1: the report must swallow the failing status.
fn exit1_shim(dir: &Path, name: &str) -> PathBuf {
    write_shim(
        dir,
        name,
        "echo \"$@\" >> \"$(dirname \"$0\")/report.log\"\nexit 1",
    )
}

/// Logs its argv, then sleeps past the report timeout. `exec` so the kill
/// takes the whole process.
fn hang_shim(dir: &Path, name: &str) -> PathBuf {
    // Log the argv FIRST, then hang: the log line proves the report was
    // attempted while `exec sleep 30` keeps the process alive past
    // REPORT_TIMEOUT, so the kill path is what ends it.
    write_shim(
        dir,
        name,
        "echo \"$@\" >> \"$(dirname \"$0\")/report.log\"; exec sleep 30",
    )
}

/// Every line the shims in `dir` recorded so far (empty when none).
fn report_lines(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("report.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Poll `report_lines` until at least `want` lines exist or `timeout` elapses.
fn wait_for_lines(dir: &Path, want: usize, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let lines = report_lines(dir);
        if lines.len() >= want {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "shim log stalled at {} lines (want {want})",
            lines.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The `--seq` value of one recorded report line (the last token).
fn seq_of(line: &str) -> u64 {
    line.split_whitespace()
        .last()
        .expect("seq token")
        .parse::<u64>()
        .expect("seq is numeric")
}

/// Wall clock in epoch-ms, the base the reporter's seq has to sit on.
fn epoch_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is past the epoch")
            .as_millis(),
    )
    .expect("epoch-ms fits u64")
}

/// The `--state` value of one recorded report line.
fn state_of(line: &str) -> Option<&str> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    tokens
        .iter()
        .position(|t| *t == "--state")
        .and_then(|i| tokens.get(i + 1).copied())
}

/// Assert one recorded report line has the settled argv shape
/// `pane report-agent <pane> --source clauth --agent claude --state <state>
/// --seq <n>` and return its seq.
fn assert_report_shape(line: &str, pane_id: &str, state: &str) -> u64 {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(
        &tokens[..5],
        ["pane", "report-agent", pane_id, "--source", "clauth"],
        "argv order (pane id first, herdr's parser takes args[0]): {line}"
    );
    assert_eq!(
        &tokens[5..9],
        ["--agent", "claude", "--state", state],
        "argv: {line}"
    );
    assert_eq!(tokens[9], "--seq", "argv: {line}");
    tokens[10].parse::<u64>().expect("seq is numeric")
}

/// A `DelegateArgs` with every optional field unset and JSON format.
fn base() -> DelegateArgs {
    DelegateArgs {
        profiles: None,
        prompt: None,
        prompt_file: None,
        model: None,
        cwd: None,
        env: None,
        args: None,
        timeout_secs: None,
        idle_secs: None,
        resume: None,
        isolated: None,
        background: None,
    }
}

/// Seed `names` on disk in ONE config (each seed saves the app state, so a
/// second call would drop the first name), optionally disabling each. A
/// disabled target passes the handler (so the sync delegate commits to spawn)
/// while `run_delegate` refuses it before any `claude` spawn; an enabled
/// target pairs with a nonexistent `cwd` to stop a background task at the cwd
/// gate.
fn seed(names: &[&str], disabled: bool) {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    for name in names {
        crate::actions::create_blank_profile(&mut config, (*name).to_string(), None, None, None)
            .expect("create profile");
    }
    if disabled {
        for name in names {
            crate::actions::disable_profile(&mut config, name).expect("disable profile");
        }
    }
}

/// Drive the async `delegate` tool with `CLAUTH_MCP_DEPTH` cleared, mirroring
/// `mcp_delegate_args::call_delegate` (same serialization rationale).
fn drive(server: &ClauthServer, args: DelegateArgs) -> CallToolResult {
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the sandbox's HOME_TEST_LOCK.
    unsafe { std::env::remove_var(MCP_DEPTH_ENV) };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let result = rt.block_on(async { server.delegate_with(args, ProgressSink::none()).await });

    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }
    result.expect("delegate returns a tool result, never a transport error")
}

/// Construct a server with a reporter resolved under the pinned env, then
/// DROP the pin before returning. The server must keep reporting: a per-call
/// re-resolution would now see the ambient env (no shim), so the recorded
/// shim lines are the construction-time pin.
fn pinned_server(home: &HomeSandbox, pane: &str, shim: &Path) -> ClauthServer {
    let _pin = EnvPin::new(home, Some(pane), Some(shim));
    ClauthServer::new().with_herdr_pane(PaneReporter::resolve())
}

/// Hold the sandbox until `count` jobs reach done, so their `write_done`
/// lands under the sandbox and never the real `~/.clauth` (mirrors
/// `mcp_delegate_args::wait_for_jobs_done`).
fn wait_for_jobs_done(count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = jobs::jobs_dir()
            .ok()
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let id = e.path().file_stem()?.to_str()?.to_string();
                        jobs::read(&id).map(|r| r.state == jobs::JobState::Done)
                    })
                    .filter(|done| *done)
                    .count()
            })
            .unwrap_or(0);
        if done >= count {
            return;
        }
        assert!(Instant::now() < deadline, "jobs never reached done");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ── gating: what `PaneReporter::resolve` accepts ─────────────────────────────

#[test]
fn resolve_requires_pane_id() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _pin = EnvPin::new(&home, None, Some(&shim));
    assert!(PaneReporter::resolve().is_none(), "no pane id, no reporter");
}

#[test]
fn resolve_requires_resolvable_binary() {
    let home = HomeSandbox::new();
    let missing = home.home().join("absent-herdr");
    let _pin = EnvPin::new(&home, Some("pane-7"), Some(&missing));
    assert!(
        PaneReporter::resolve().is_none(),
        "a pane id without a resolvable herdr binary is a no-op"
    );
}

#[test]
fn resolve_accepts_path_binary() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _pin = EnvPin::new(&home, Some("pane-7"), Some(&shim));
    assert!(
        PaneReporter::resolve().is_some(),
        "pane id + HERDR_BIN_PATH resolves"
    );
}

#[test]
fn resolve_finds_bare_name_on_path() {
    let home = HomeSandbox::new();
    // Written to disk but not pinned: the PATH search below must find it by
    // name alone, with HERDR_BIN_PATH unset.
    let _shim = echo_shim(home.home(), "herdr");
    let _pin = EnvPin::new(&home, Some("pane-7"), None);
    // Bare-name resolution searches PATH: prepend the shim's dir so it wins
    // over any real herdr on the ambient PATH.
    let saved = std::env::var_os("PATH");
    // SAFETY: test-only, serialized by the sandbox's HOME_TEST_LOCK.
    unsafe {
        let mut joined = std::ffi::OsString::from(home.home());
        if let Some(rest) = &saved {
            joined.push(":");
            joined.push(rest);
        }
        std::env::set_var("PATH", joined);
    }
    let resolved = PaneReporter::resolve();
    // SAFETY: same as above — restore.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }
    assert!(
        resolved.is_some(),
        "a bare `herdr` on PATH resolves (HERDR_BIN_PATH unset)"
    );
}

// ── sync delegate ────────────────────────────────────────────────────────────

#[test]
fn sync_delegate_reports_working_then_idle() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    seed(&["sad"], true);
    let server = pinned_server(&home, "pane-7", &shim);
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["sad".to_string()]),
            prompt: Some("hi".into()),
            ..base()
        },
    );
    // The delegate still refuses the disabled target; the reports ride along.
    assert_eq!(
        result.is_error,
        Some(true),
        "the run's own error is untouched"
    );
    let lines = report_lines(home.home());
    assert_eq!(lines.len(), 2, "working then idle, exactly: {lines:?}");
    let seq1 = assert_report_shape(&lines[0], "pane-7", "working");
    let seq2 = assert_report_shape(&lines[1], "pane-7", "idle");
    assert!(
        seq2 > seq1,
        "seq increases across reports: {seq1} then {seq2}"
    );
}

#[test]
fn refusal_before_commit_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let server = pinned_server(&home, "pane-7", &shim);
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["nope".to_string()]),
            prompt: Some("hi".into()),
            ..base()
        },
    );
    assert_eq!(result.is_error, Some(true), "unknown profile refuses");
    assert!(
        report_lines(home.home()).is_empty(),
        "a refusal never commits, so no report: {:?}",
        report_lines(home.home())
    );
}

#[test]
fn failing_herdr_does_not_fail_the_delegate() {
    let home = HomeSandbox::new();
    let shim = exit1_shim(home.home(), "herdr");
    seed(&["sad"], true);
    let server = pinned_server(&home, "pane-7", &shim);
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["sad".to_string()]),
            prompt: Some("hi".into()),
            ..base()
        },
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "the delegate error envelope survives a failing herdr"
    );
    let lines = report_lines(home.home());
    assert_eq!(
        lines.len(),
        2,
        "both reports were attempted and the exit status swallowed: {lines:?}"
    );
}

#[test]
fn hanging_herdr_does_not_block_the_delegate() {
    let home = HomeSandbox::new();
    let shim = hang_shim(home.home(), "herdr");
    seed(&["sad"], true);
    let server = pinned_server(&home, "pane-7", &shim);
    let start = Instant::now();
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["sad".to_string()]),
            prompt: Some("hi".into()),
            ..base()
        },
    );
    let elapsed = start.elapsed();
    assert_eq!(
        result.is_error,
        Some(true),
        "the delegate result still lands"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the report timeout bounds a hung herdr (two reports): {elapsed:?}"
    );
    let lines = report_lines(home.home());
    assert_eq!(
        lines.len(),
        2,
        "both reports were attempted, killed on the timeout, and the delegate was unaffected: {lines:?}"
    );
    assert_report_shape(&lines[0], "pane-7", "working");
    assert_report_shape(&lines[1], "pane-7", "idle");
}

// ── background delegate ──────────────────────────────────────────────────────

#[test]
fn background_delegate_reports_working_then_idle() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    seed(&["bee"], false);
    let server = pinned_server(&home, "pane-7", &shim);
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["bee".to_string()]),
            prompt: Some("hi".into()),
            background: Some(true),
            // Nonexistent: stops the detached task at the cwd gate, before
            // any `claude` spawn.
            cwd: Some(
                home.home()
                    .join("does-not-exist")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..base()
        },
    );
    assert_ne!(result.is_error, Some(true), "the job is accepted");
    // The idle report fires from the detached task after the job finalizes.
    let lines = wait_for_lines(home.home(), 2, Duration::from_secs(20));
    assert_eq!(
        lines.len(),
        2,
        "working at commit, idle at finalize: {lines:?}"
    );
    assert_report_shape(&lines[0], "pane-7", "working");
    assert_report_shape(&lines[1], "pane-7", "idle");
}

#[test]
fn fanout_reports_working_once_and_idle_last() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    seed(&["bee1", "bee2"], false);
    let server = pinned_server(&home, "pane-7", &shim);
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["bee1".into(), "bee2".into()]),
            prompt: Some("hi".into()),
            background: Some(true),
            cwd: Some(
                home.home()
                    .join("does-not-exist")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..base()
        },
    );
    assert_ne!(result.is_error, Some(true), "the fan-out is accepted");
    // Both finalizes precede their task's idle report (the end-guard drops
    // after `write_done`), so both episodes are paired off once every working
    // has its idle. Waiting on the LAST APPENDED line instead would be a race:
    // `report` holds no lock, two of them run at once, and the shim writing
    // second is not the report minted second.
    wait_for_jobs_done(2);
    let deadline = Instant::now() + Duration::from_secs(20);
    let lines = loop {
        let mut lines = report_lines(home.home());
        lines.sort_by_key(|line| seq_of(line));
        let idles = lines.iter().filter(|l| state_of(l) == Some("idle")).count();
        if idles > 0 && idles * 2 == lines.len() {
            break lines;
        }
        assert!(
            Instant::now() < deadline,
            "reports never paired off after both jobs finalized: {lines:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    // Two in-flight episodes (tasks overlap or not): one working + one idle
    // per episode, so two or four lines — the count transitions decide.
    assert!(
        lines.len() == 2 || lines.len() == 4,
        "one working per begin, one idle per end: {lines:?}"
    );
    // Read in seq order, which is the order herdr reads them in.
    assert_report_shape(&lines[0], "pane-7", "working");
    assert_report_shape(lines.last().expect("idle line"), "pane-7", "idle");
    let states: Vec<Option<&str>> = lines.iter().map(|l| state_of(l)).collect();
    assert!(
        states.windows(2).all(|w| w[0] != w[1]),
        "states alternate in seq order, one per transition: {lines:?}"
    );
    let unique: std::collections::HashSet<u64> = lines.iter().map(|l| seq_of(l)).collect();
    assert_eq!(
        unique.len(),
        lines.len(),
        "seq is strictly monotonic across reports: {lines:?}"
    );
}

// ── counter + no-op paths ────────────────────────────────────────────────────

#[test]
fn overlap_reports_idle_once_after_last_end() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let reporter = {
        let _pin = EnvPin::new(&home, Some("pane-9"), Some(&shim));
        PaneReporter::resolve().expect("pane env resolves a reporter")
    };
    // Two overlapping delegates: one working report on the 0→1 transition...
    let g1 = InFlightGuard::begin(&reporter);
    let g2 = InFlightGuard::begin(&reporter);
    let lines = report_lines(home.home());
    assert_eq!(
        lines.len(),
        1,
        "one working for two overlapping runs: {lines:?}"
    );
    assert_report_shape(&lines[0], "pane-9", "working");
    drop(g1);
    assert_eq!(
        report_lines(home.home()).len(),
        1,
        "no idle while one delegate is still in flight"
    );
    // ...and one idle, after the LAST finalize.
    drop(g2);
    let lines = report_lines(home.home());
    assert_eq!(lines.len(), 2, "working then idle, exactly: {lines:?}");
    assert_report_shape(&lines[1], "pane-9", "idle");
}

/// A reporter over a shim, resolved under a pin the caller does not keep.
fn pinned_reporter(home: &HomeSandbox, pane: &str, shim: &Path) -> PaneReporter {
    let _pin = EnvPin::new(home, Some(pane), Some(shim));
    PaneReporter::resolve().expect("pane env + shim resolves a reporter")
}

// ── seq clock ────────────────────────────────────────────────────────────────

#[test]
fn seq_starts_from_the_clock_so_a_restart_cannot_rewind() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let reporter = pinned_reporter(&home, "pane-9", &shim);
    // herdr's high-water for `--source clauth` outlives this process, so a
    // fresh reporter's first seq has to beat whatever the last one left there.
    // Through the transition the report path actually takes, never a clock the
    // shipped code could stop calling.
    let floor = epoch_ms();
    let seq = reporter.enter_for_test().expect("the 0→1 transition mints");
    assert!(
        seq >= floor,
        "the first seq of a fresh process reads the clock, not a counter: {seq} < {floor}"
    );
    assert!(
        seq <= floor + 60_000,
        "the first seq is this clock, not an arbitrary ceiling: {seq}"
    );
    reporter.leave_for_test().expect("the →0 transition mints");
}

#[test]
fn a_same_millisecond_pair_still_separates() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let reporter = pinned_reporter(&home, "pane-9", &shim);
    // Fifty transitions land inside one millisecond, where the clock alone
    // repeats and herdr would drop every report after the first.
    let mut seqs = Vec::new();
    for _ in 0..25 {
        seqs.push(reporter.enter_for_test().expect("0→1 mints"));
        seqs.push(reporter.leave_for_test().expect("→0 mints"));
    }
    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "seq is strictly increasing inside one millisecond: {seqs:?}"
    );
}

// ── transition ordering ──────────────────────────────────────────────────────

#[test]
fn a_hung_report_does_not_block_the_next_transition() {
    let home = HomeSandbox::new();
    let shim = hang_shim(home.home(), "herdr");
    let reporter = pinned_reporter(&home, "pane-9", &shim);
    let blocked = reporter.clone();
    let worker = std::thread::spawn(move || blocked.begin());
    // The shim logs before it hangs, so one line means the report is in
    // flight: the only window where a lock held across the spawn would bite.
    wait_for_lines(home.home(), 1, Duration::from_secs(10));
    let start = Instant::now();
    reporter.begin();
    let elapsed = start.elapsed();
    worker.join().expect("the blocked report thread finishes");
    assert!(
        elapsed < Duration::from_millis(500),
        "a second delegate's transition waited out the hung report: {elapsed:?}"
    );
    assert_eq!(
        report_lines(home.home()).len(),
        1,
        "the 1→2 transition reports nothing of its own"
    );
}

#[test]
fn transition_order_and_seq_order_agree_under_contention() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let reporter = pinned_reporter(&home, "pane-9", &shim);
    // Transitions with NO report between them. Every report costs a subprocess
    // spawn, milliseconds against the sub-microsecond window a seq minted off
    // its decision inverts in, so a fixture that reports cannot reach this at
    // all: the spawn swamps the race it is supposed to observe.
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let reporter = reporter.clone();
            let events = std::sync::Arc::clone(&events);
            std::thread::spawn(move || {
                let mut local = Vec::new();
                for _ in 0..20_000 {
                    if let Some(seq) = reporter.enter_for_test() {
                        local.push(("working", seq));
                    }
                    if let Some(seq) = reporter.leave_for_test() {
                        local.push(("idle", seq));
                    }
                }
                events.lock().expect("events lock").extend(local);
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker finishes");
    }

    let mut events = events.lock().expect("events lock").clone();
    assert!(
        events.len() > 100,
        "the fixture has to produce transitions before their order means anything: {}",
        events.len()
    );
    // herdr replays by seq and keeps the newest, so seq order IS the order it
    // sees. Transitions alternate by construction (no second 0→1 without a →0
    // between), so anything but an alternating read means a pair reached herdr
    // reversed: an `idle` outranking the `working` that followed it leaves the
    // pane idle with a delegate running.
    events.sort_by_key(|(_, seq)| *seq);
    // Uniqueness first: a duplicate seq is its own defect (herdr's high-water
    // drops the second), and checking it here keeps it from reaching the
    // ordering assertion as a same-state pair and reading as an inversion.
    let unique: std::collections::HashSet<u64> = events.iter().map(|(_, seq)| *seq).collect();
    assert_eq!(
        unique.len(),
        events.len(),
        "two transitions sharing a seq lose one to herdr's high-water"
    );
    let inversion = events.windows(2).position(|w| w[0].0 == w[1].0);
    assert!(
        inversion.is_none(),
        "seq order disagrees with transition order at event {:?}: {:?}",
        inversion,
        inversion.map(|i| &events[i..(i + 2).min(events.len())])
    );
}

// ── unpaired end ─────────────────────────────────────────────────────────────

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "end with no matching begin")]
fn unpaired_end_trips_the_debug_assert() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let reporter = pinned_reporter(&home, "pane-9", &shim);
    drop(InFlightGuard::end_only(reporter));
}

#[cfg(not(debug_assertions))]
#[test]
fn unpaired_end_leaves_the_count_usable() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let reporter = pinned_reporter(&home, "pane-9", &shim);
    drop(InFlightGuard::end_only(reporter.clone()));
    assert!(
        report_lines(home.home()).is_empty(),
        "an end with nothing in flight reports nothing"
    );
    // An underflowed count would swallow every later working report.
    reporter.begin();
    let lines = report_lines(home.home());
    assert_eq!(lines.len(), 1, "the next delegate still reports: {lines:?}");
    assert_report_shape(&lines[0], "pane-9", "working");
}

#[test]
fn server_without_pane_env_spawns_nothing() {
    let home = HomeSandbox::new();
    seed(&["sad"], true);
    // Pin BOTH vars off: no pane id means the serve path resolves no
    // reporter, and the drive must spawn nothing, shim or real herdr.
    let _pin = EnvPin::new(&home, None, None);
    let server = ClauthServer::new();
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["sad".to_string()]),
            prompt: Some("hi".into()),
            ..base()
        },
    );
    assert_eq!(result.is_error, Some(true), "the delegate still runs");
    assert!(
        report_lines(home.home()).is_empty(),
        "no pane, no reports, no spawns"
    );
}

#[test]
fn server_without_reporter_spawns_nothing_even_with_pane_env() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    seed(&["sad"], true);
    // A server built without `with_herdr_pane` never reports, whatever the
    // ambient env says — this is what keeps the rest of the suite safe when
    // the gate runs inside a real herdr pane.
    let _pin = EnvPin::new(&home, Some("pane-7"), Some(&shim));
    let server = ClauthServer::new();
    let result = drive(
        &server,
        DelegateArgs {
            profiles: Some(vec!["sad".to_string()]),
            prompt: Some("hi".into()),
            ..base()
        },
    );
    assert_eq!(result.is_error, Some(true), "the delegate still runs");
    assert!(
        report_lines(home.home()).is_empty(),
        "a plain ClauthServer::new() carries no reporter"
    );
}
