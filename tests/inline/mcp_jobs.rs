#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Disk job-store coverage: atomic write/read roundtrip, id safety, eviction,
//! and GC of expired / orphaned / oversized state. Home-sandboxed so files land
//! in a tempdir, never the real `~/.clauth/jobs`.

use super::*;
use crate::testutil::HomeSandbox;

/// The running spec every test writes through, so one signature change lands in
/// one place.
///
/// It is the STREAMING shape, which is what `reserve_background_job` writes for
/// a default `delegate({background: true})`: no wall clock, an idle guard at the
/// default. The pair `(3600, Some(300))` this used to carry is one the producer
/// can no longer emit, so every test in the file inherited a run that cannot
/// exist.
/// `recorded_at` equals `started_at` here because that is what a job which
/// STARTED background carries — the reserve mints the record at the run's own
/// birth. A run handed off mid-flight is the shape where the two differ, and
/// the tests about that difference set it apart deliberately.
fn spec(job_id: &str, profile: &str, started_at: u64) -> RunningSpec {
    RunningSpec {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        started_at,
        recorded_at: started_at,
        timeout_secs: 0,
        idle_secs: Some(300),
    }
}

/// The other shape the producer emits: a caller-pinned `--output-format`, where
/// the idle leg is off and the wall clock is the only deadline.
fn pinned_format_spec(job_id: &str, profile: &str, started_at: u64) -> RunningSpec {
    RunningSpec {
        timeout_secs: 900,
        idle_secs: None,
        ..spec(job_id, profile, started_at)
    }
}

#[test]
fn write_read_roundtrip_running_then_done() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_running(&spec(&id, "work", 1000)).unwrap();

    let r = read(&id).expect("running record");
    assert_eq!(r.state, JobState::Running);
    assert_eq!(r.profile, "work");
    assert!(r.envelope.is_none());

    let env = serde_json::json!({ "is_error": false, "result": "ok" });
    write_done(&id, "work", 1000, env.clone()).unwrap();
    let r = read(&id).expect("done record");
    assert_eq!(r.state, JobState::Done);
    assert_eq!(r.envelope, Some(env));

    // an atomic write leaves no .tmp behind.
    let tmp_left = std::fs::read_dir(jobs_dir().unwrap())
        .unwrap()
        .flatten()
        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"));
    assert!(!tmp_left, "atomic write leaves no .tmp");

    remove(&id);
    assert!(read(&id).is_none(), "removed job is gone");
}

/// A running job file is written by the server that spawned it and read by a
/// possibly newer one PLUS the separate `mcp-await-job` hook process, so every
/// field added after the first release has to default. Pinned against the real
/// bytes an older server wrote, not a hand-built `JobRecord`: a struct literal
/// would compile against whatever the fields are today and prove nothing about
/// the wire. `read` swallows a parse failure as `None`, which reaches the caller
/// as `unknown job_id` on a job that is running fine.
#[test]
fn a_job_file_from_an_older_server_still_parses() {
    let _home = HomeSandbox::new();
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-legacy-0.json"),
        br#"{"job_id":"d-legacy-0","profile":"work","state":"running","started_at":1000}"#,
    )
    .unwrap();

    let r = read("d-legacy-0").expect("a pre-slice-2 running file still parses");
    assert_eq!(r.state, JobState::Running);
    assert_eq!(r.timeout_secs, 0, "no deadline recorded by that server");
    assert_eq!(r.idle_secs, None);
    assert_eq!(r.last_output_at, 0);
    assert_eq!(r.tail, "");
    assert_eq!(r.done_at, 0, "no finish stamp either");
}

/// A heartbeat rewrites the SAME running record: the identity and whichever
/// deadline that run has survive it, and only the liveness fields move.
///
/// Both producible shapes, because each proves the half the other cannot — a
/// streaming record's absent wall is a serialized default, so only the
/// pinned-format one can show a real figure carried through.
#[test]
fn a_heartbeat_rewrites_the_running_record_in_place() {
    let _home = HomeSandbox::new();
    for (id, spec) in [
        ("d-beat-0", spec("d-beat-0", "work", 1000)),
        ("d-beat-1", pinned_format_spec("d-beat-1", "work", 1000)),
    ] {
        write_running(&spec).unwrap();
        let fresh = read(id).expect("running record");
        assert_eq!(fresh.last_output_at, 0, "nothing has arrived yet");
        assert_eq!(fresh.tail, "");

        // Epoch ms, the same anchor `started_at` uses — a run-relative stamp
        // would silently disagree with it by the acquire+spawn latency.
        write_heartbeat(&spec, 41_000, "moving to the fallback tests").unwrap();
        let beaten = read(id).expect("heartbeat record");
        assert_eq!(beaten.state, JobState::Running, "still the same job");
        assert_eq!(beaten.job_id, id);
        assert_eq!(beaten.profile, "work");
        assert_eq!(beaten.started_at, 1000);
        assert_eq!(beaten.timeout_secs, spec.timeout_secs);
        assert_eq!(beaten.idle_secs, spec.idle_secs);
        assert_eq!(beaten.last_output_at, 41_000);
        assert_eq!(beaten.tail, "moving to the fallback tests");
        assert!(beaten.envelope.is_none());
    }
}

#[test]
fn unknown_job_reads_none() {
    let _home = HomeSandbox::new();
    assert!(read("d-1-999").is_none());
}

#[test]
fn job_id_safety_rejects_traversal_and_separators() {
    assert!(is_safe_job_id("d-123-4"));
    assert!(is_safe_job_id("abc_DEF-9"));
    assert!(!is_safe_job_id(""));
    assert!(!is_safe_job_id("../escape"));
    assert!(!is_safe_job_id("a/b"));
    assert!(!is_safe_job_id("a.json"));
    assert!(!is_safe_job_id(&"x".repeat(200)));
}

#[test]
fn new_job_id_is_unique_and_safe() {
    let a = new_job_id(5);
    let b = new_job_id(5);
    assert_ne!(a, b, "same-ms ids differ via the counter");
    assert!(is_safe_job_id(&a) && is_safe_job_id(&b));
}

/// `base36`'s buffer is sized by an argument no wall-clock stamp can ever
/// exercise, so passing the widest input at all is what reds an under-sized one
/// — by panicking before any assertion here runs, on the index in release and
/// on the counter's own underflow in debug. An OVER-sized buffer is invisible to
/// this test and costs nothing but stack, so nothing below claims to catch it:
/// the two assertions pin the encoding, not the sizing.
#[test]
fn base36_spans_its_whole_domain() {
    assert_eq!(base36(0), "0", "zero is a digit, never the empty string");
    let widest = base36(u64::MAX);
    assert_eq!(widest.len(), 13, "u64::MAX spells 13 base-36 digits");
    assert_eq!(u64::from_str_radix(&widest, 36), Ok(u64::MAX));
}

/// Write a `done` file with an explicit `done_at`, as raw bytes: `write_done`
/// stamps the real clock, and the retention rule under test is about a stamp a
/// test has to choose. Omitting `done_at` writes the pre-`done_at` shape.
fn seed_done_at(job_id: &str, started_at: u64, done_at: Option<u64>) {
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let mut record = serde_json::json!({
        "job_id": job_id,
        "profile": "p",
        "state": "done",
        "started_at": started_at,
        "envelope": { "result": "kept" },
    });
    if let Some(at) = done_at {
        record["done_at"] = serde_json::json!(at);
    }
    std::fs::write(
        dir.join(format!("{job_id}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

#[test]
fn gc_reaps_expired_running_and_done_keeps_fresh() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64; // far-future ms so "fresh" entries read as recent

    seed_done_at("d-fresh-done", now, Some(now));
    write_running(&spec("d-fresh-run", "p", now)).unwrap();
    seed_done_at(
        "d-old-done",
        now - DONE_TTL_MS - 1,
        Some(now - DONE_TTL_MS - 1),
    );
    write_running(&spec("d-old-run", "p", now - RUNNING_TTL_MS - 1)).unwrap();

    gc(now);

    assert!(read("d-fresh-done").is_some());
    assert!(read("d-fresh-run").is_some());
    assert!(read("d-old-done").is_none(), "expired done reaped");
    assert!(read("d-old-run").is_none(), "orphaned running reaped");
}

/// The Done TTL retains a file for an hour after it FINISHES, which is what its
/// own doc promises a slow poller. Measured from the mint instead, any delegate
/// that ran for over an hour is already expired the instant it finalizes, and
/// the next sweep destroys the salvage envelope.
#[test]
fn the_done_ttl_measures_from_the_finish_not_the_mint() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;

    // Minted two hours ago, finished a moment ago: a run killed at its wall
    // clock, or any long delegate.
    seed_done_at("d-long-run", now - 2 * DONE_TTL_MS, Some(now - 1000));
    // Minted a moment ago, finished over an hour ago — impossible in practice,
    // but it pins that the FINISH is what the rule reads.
    seed_done_at("d-stale-finish", now - 1000, Some(now - DONE_TTL_MS - 1));
    // No `done_at` at all: a file an older server wrote, which must keep
    // exactly the mint-anchored behaviour rather than becoming immortal.
    seed_done_at("d-legacy-done", now - DONE_TTL_MS - 1, None);
    seed_done_at("d-legacy-fresh", now - 1000, None);

    gc(now);

    assert!(
        read("d-long-run").is_some(),
        "a long run's envelope survives its own length"
    );
    assert!(read("d-stale-finish").is_none(), "an hour past the finish");
    assert!(read("d-legacy-done").is_none(), "old file, old rule");
    assert!(read("d-legacy-fresh").is_some(), "old file, still fresh");
}

/// A streaming delegate has no wall clock, so "older than the max delegate
/// lifetime" bounds nothing any more: anchored on the mint, a run still healthy
/// past the TTL had its `running` file deleted under it — at startup AND by the
/// sweep every `monitor` collect runs — and the job then answered `unknown
/// job_id` while its child kept spending the account. What separates a corpse
/// from a long run is SILENCE, not age: a live background run rewrites this file
/// on every heartbeat and cannot go quiet for longer than its own idle guard
/// without being killed.
#[test]
fn a_job_still_talking_survives_the_corpse_sweep_however_old_it_is() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let ancient = now - 10 * RUNNING_TTL_MS;

    // Minted half a day ago, said something a second ago: alive.
    write_heartbeat(&spec("d-talking", "p", ancient), now - 1000, "still going").unwrap();
    // Same age, never heard from since the mint: a corpse.
    write_running(&spec("d-silent", "p", ancient)).unwrap();

    gc_running_corpses(now);

    assert!(
        read("d-talking").is_some(),
        "a heartbeat inside the window is liveness, whatever the run's total age"
    );
    assert!(
        read("d-silent").is_none(),
        "silence past the window is still a corpse"
    );

    // The startup sweep reads the same anchor, and it is the one that runs while
    // ANOTHER server's jobs are in flight.
    write_heartbeat(
        &spec("d-talking-2", "p", ancient),
        now - 1000,
        "still going",
    )
    .unwrap();
    gc(now);
    assert!(
        read("d-talking-2").is_some(),
        "startup GC must not reap a live run a sibling server is still driving"
    );
}

/// The collect path runs a NARROWER sweep than startup: it reaps only the
/// `running` corpses a dead server orphaned, which is the whole reason finding 6
/// wanted a sweep there. Reaping `done` before a read destroys the envelope the
/// call came for, and the `.tmp` sweep and retention cap buy nothing at all.
#[test]
fn the_corpse_sweep_touches_only_orphaned_running_files() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;

    seed_done_at(
        "d-expired-done",
        now - 2 * DONE_TTL_MS,
        Some(now - 2 * DONE_TTL_MS),
    );
    write_running(&spec("d-live-run", "p", now)).unwrap();
    write_running(&spec("d-corpse-run", "p", now - RUNNING_TTL_MS - 1)).unwrap();
    let dir = jobs_dir().unwrap();
    std::fs::write(dir.join("d-9-9.json.tmp"), b"partial").unwrap();

    gc_running_corpses(now);

    assert!(
        read("d-corpse-run").is_none(),
        "a dead server's file is a corpse"
    );
    assert!(read("d-live-run").is_some(), "a live job is untouched");
    assert!(
        read("d-expired-done").is_some(),
        "a collect must never destroy a result, whatever its age"
    );
    assert!(
        dir.join("d-9-9.json.tmp").exists(),
        "the tmp sweep is startup's job, not a reader's"
    );
}

#[test]
fn gc_sweeps_stray_tmp_files() {
    let _home = HomeSandbox::new();
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("d-1-0.json.tmp"), b"partial").unwrap();
    gc(0);
    assert!(!dir.join("d-1-0.json.tmp").exists(), "stray tmp swept");
}

#[test]
fn gc_caps_retained_to_newest() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let total = MAX_RETAINED + 5;
    for i in 0..total {
        // Both stamps rise with i, so low i are the oldest either way and the
        // cap's ordering is the only thing under test here. `write_done` would
        // stamp every finish at the real clock, which leaves 261 records the
        // cap cannot order at all.
        let age = total as u64 - i as u64;
        seed_done_at(&format!("d-cap-{i}"), now - age, Some(now - age));
    }
    gc(now);

    let remaining = std::fs::read_dir(jobs_dir().unwrap())
        .unwrap()
        .flatten()
        .count();
    assert_eq!(remaining, MAX_RETAINED, "capped to MAX_RETAINED newest");
    assert!(read("d-cap-0").is_none(), "oldest reaped");
    assert!(
        read(&format!("d-cap-{}", total - 1)).is_some(),
        "newest kept"
    );
}

/// The cap keeps the newest jobs by the same stamp the TTL retains from: a long
/// delegate's fresh, never-read result must not be evicted ahead of a short
/// run's older one just because it was minted earlier.
#[test]
fn the_retention_cap_keeps_the_newest_finish_not_the_newest_mint() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;

    // The long run: minted an hour before the rest, finished most recently.
    seed_done_at("d-long", now - 3_600_000, Some(now - 60_000));
    // The short runs: all minted in the last 30s, all finished before it.
    for i in 0..MAX_RETAINED {
        seed_done_at(
            &format!("d-short-{i}"),
            now - 30_000 + i as u64,
            Some(now - 300_000 + i as u64),
        );
    }

    gc(now); // MAX_RETAINED + 1 files: exactly one is evicted

    assert!(
        read("d-long").is_some(),
        "the newest RESULT survives the cap, whatever its mint"
    );
    assert!(
        read("d-short-0").is_none(),
        "the oldest result is the one evicted"
    );
    assert_eq!(
        std::fs::read_dir(jobs_dir().unwrap()).unwrap().count(),
        MAX_RETAINED,
        "capped to MAX_RETAINED"
    );
}

/// A job file carries the delegate's prompt and the account's full response, and
/// the dir naming every background job is as readable as the files in it. Both
/// ride clauth's owner-only rule for `~/.clauth`.
#[cfg(unix)]
#[test]
fn job_files_and_dir_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_done(
        &id,
        "work",
        1000,
        serde_json::json!({"result": "secret output"}),
    )
    .unwrap();

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let dir = jobs_dir().unwrap();
    assert_eq!(
        mode(&dir),
        0o700,
        "jobs dir mode should be 0o700, got {:#o}",
        mode(&dir)
    );
    let file = dir.join(format!("{id}.json"));
    assert_eq!(
        mode(&file),
        0o600,
        "job file mode should be 0o600, got {:#o}",
        mode(&file)
    );
}
