#![allow(unsafe_code)]
use super::*;
use std::fs;
use std::time::{Duration, SystemTime};

use crate::testutil::{HomeSandbox, set_mtime};

// V1 expires_at < V2 so tie-break tests can assert which side wins unambiguously.
const CREDS_V1: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok1","expiresAt":1000}}"#;
const CREDS_V2: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok2","expiresAt":2000}}"#;

#[test]
fn sync_no_op_when_link_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert!(!canonical.exists());
}

#[cfg(unix)]
#[test]
fn sync_no_op_when_link_is_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    std::os::unix::fs::symlink(&canonical, &link_path).expect("symlink");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_skips_invalid_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, b"not json").expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    // link stayed a regular file — waiting for CC's write to complete
    let meta = link_path.symlink_metadata().expect("meta");
    assert!(!meta.file_type().is_symlink());
}

#[test]
fn sync_skips_empty_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    // {} parses as ClaudeCredentials but carries no OAuth token — treat as partial
    fs::write(&link_path, b"{}").expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn sync_relinks_when_content_matches_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_writes_canonical_when_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V2).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    let base = SystemTime::now(); // runtime is newer → wins mtime tie-break
    set_mtime(&canonical, base);
    set_mtime(&link_path, base + Duration::from_secs(5));
    assert!(sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V2);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_creates_canonical_when_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    assert!(sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

// ── expires_at tie-breaking in sync_credentials_unlocked ─────────────────────

#[test]
fn sync_no_write_when_bytes_identical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(!written, "no write when bytes identical");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

// Canonical newer → canonical wins; mtime is primary (expires_at agrees: V2 > V1).
#[test]
fn sync_canonical_wins_when_written_more_recently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime (stale)");
    fs::write(&canonical, CREDS_V2).expect("write canonical (rotated)");
    let base = SystemTime::now(); // canonical strictly newer
    set_mtime(&link_path, base);
    set_mtime(&canonical, base + Duration::from_secs(5));

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        !written,
        "canonical must not be overwritten when it is the more recent write"
    );
    assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink(),
        "runtime re-linked to canonical even when canonical wins"
    );
}

// Runtime newer → runtime wins; mtime is primary (expires_at agrees: V2 > V1).
#[test]
fn sync_runtime_wins_when_written_more_recently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V2).expect("write runtime (newer)");
    fs::write(&canonical, CREDS_V1).expect("write canonical (older)");
    let base = SystemTime::now();
    set_mtime(&canonical, base);
    set_mtime(&link_path, base + Duration::from_secs(5));

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        written,
        "canonical must be overwritten when runtime is the more recent write"
    );
    assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
}

// Bug fix: rotate-all can stamp a canonical token with later expires_at than a
// fresh CC re-login written after. mtime must decide — not expires_at — or the
// watchdog silently discards the user's just-completed login and burns its chain.
#[test]
fn sync_runtime_wins_when_newer_mtime_despite_lower_expires_at() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    // canonical (rotated) has later expires_at (V2=2000); runtime (CC re-login) has V1=1000 but written last
    fs::write(&canonical, CREDS_V2).expect("write canonical (rotated, later exp)");
    fs::write(&link_path, CREDS_V1).expect("write runtime (fresh re-login)");
    let base = SystemTime::now();
    set_mtime(&canonical, base);
    set_mtime(&link_path, base + Duration::from_secs(5));

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        written,
        "runtime re-login must win on newer mtime even with lower expires_at"
    );
    assert_eq!(
        fs::read(&canonical).expect("read canonical"),
        CREDS_V1,
        "CC's fresh login bytes must be preserved into canonical, not discarded"
    );
}

// mtime tie → fall back to expires_at; canonical V2 > V1 wins, runtime re-linked.
#[test]
fn sync_falls_back_to_expires_at_on_equal_mtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime");
    fs::write(&canonical, CREDS_V2).expect("write canonical");
    let when = SystemTime::now();
    set_mtime(&link_path, when);
    set_mtime(&canonical, when);

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        !written,
        "on equal mtime, higher expires_at (canonical) wins the fallback"
    );
    assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
}

// The tie-break in isolation, no filesystem: mtime is primary, expires_at only
// breaks an equal/missing-mtime tie, and an absent canonical always yields.
#[test]
fn resolve_credential_winner_prefers_recency_then_expiry() {
    let early = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let late = SystemTime::UNIX_EPOCH + Duration::from_secs(200);

    // Newer runtime mtime wins even with a later canonical expiry.
    assert!(!resolve_credential_winner(
        Some(999),
        Some(1),
        Some(early),
        Some(late)
    ));
    // Newer canonical mtime keeps canonical despite a later runtime expiry.
    assert!(resolve_credential_winner(
        Some(1),
        Some(999),
        Some(late),
        Some(early)
    ));
    // Equal mtime → expiry tie-break; canonical wins the `>=` tie.
    assert!(resolve_credential_winner(
        Some(5),
        Some(5),
        Some(late),
        Some(late)
    ));
    // Runtime carries no token → keep canonical.
    assert!(resolve_credential_winner(Some(1), None, None, None));
    // Canonical missing/unparseable → runtime wins.
    assert!(!resolve_credential_winner(None, Some(1), None, None));
}

// Canonical absent → runtime always wins.
#[test]
fn sync_runtime_wins_when_canonical_missing_expires_at() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime");

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        written,
        "runtime must become canonical when canonical is absent"
    );
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

// Canonical unparseable → runtime wins (safer than discarding it).
#[cfg(not(target_os = "macos"))]
#[test]
fn sync_runtime_wins_when_canonical_unparseable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime");
    fs::write(&canonical, b"corrupt json {{{").expect("write corrupt canonical");

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(written, "runtime must win when canonical is unparseable");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn live_session_blocks_liveness_probe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pid_file = tmp.path().join("pid");
    let file = open_pid_file(&pid_file).expect("open");
    file.lock().expect("lock");
    assert!(is_session_alive(&pid_file));
    drop(file);
    assert!(!is_session_alive(&pid_file));
}

#[test]
fn prune_removes_dead_keeps_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let alive_path = tmp.path().join("alive");
    let dead_path = tmp.path().join("dead");
    let alive = open_pid_file(&alive_path).expect("open alive");
    alive.lock().expect("lock alive");
    fs::write(&dead_path, b"").expect("write dead");

    let count = prune_stale_sessions(tmp.path()).expect("prune");
    assert_eq!(count, 1);
    assert!(alive_path.exists());
    assert!(!dead_path.exists());
    drop(alive);
}

#[test]
fn copy_tree_replicates_files_and_subdirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("nested")).expect("mkdir");
    fs::write(src.join("a.txt"), b"hello").expect("write a");
    fs::write(src.join("nested").join("b.txt"), b"world").expect("write b");

    let dst = tmp.path().join("dst");
    copy_tree(&src, &dst).expect("copy_tree");

    assert_eq!(fs::read(dst.join("a.txt")).expect("read a"), b"hello");
    assert_eq!(
        fs::read(dst.join("nested").join("b.txt")).expect("read b"),
        b"world"
    );
}

#[test]
fn mirror_credentials_newer_runtime_wins() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    fs::write(&runtime, CREDS_V2).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V2);
}

#[test]
fn mirror_credentials_newer_canonical_wins() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V2).expect("write canonical");
    fs::write(&runtime, CREDS_V1).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&runtime, past);
    set_mtime(&canonical, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&runtime).expect("read"), CREDS_V2);
}

#[test]
fn mirror_credentials_skips_invalid_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    fs::write(&runtime, b"partial write").expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1); // canonical untouched; partial JSON ignored
}

#[test]
fn mirror_credentials_skips_empty_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    // {} parses as ClaudeCredentials but has no OAuth token
    fs::write(&runtime, b"{}").expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_credentials_seeds_missing_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&runtime, CREDS_V1).expect("write runtime");

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_tree_propagates_runtime_edit_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("todos.json"), b"[]").expect("write canonical");

    copy_tree(&claude, &runtime).expect("copy");

    // simulate CC rewriting the runtime copy
    fs::write(runtime.join("todos.json"), br#"[{"id":1}]"#).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&claude.join("todos.json"), past);
    set_mtime(&runtime.join("todos.json"), now);

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("todos.json")).expect("read canonical"),
        br#"[{"id":1}]"#
    );
}

#[test]
fn mirror_tree_skips_top_level_settings_and_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("settings.json"), br#"{"home":true}"#).expect("write h settings");
    fs::write(runtime.join("settings.json"), br#"{"runtime":true}"#).expect("write r settings");
    fs::write(claude.join(".credentials.json"), CREDS_V1).expect("write h creds");
    fs::write(runtime.join(".credentials.json"), CREDS_V2).expect("write r creds");

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(claude.join("settings.json")).expect("read"),
        br#"{"home":true}"#
    );
    assert_eq!(
        fs::read(runtime.join("settings.json")).expect("read"),
        br#"{"runtime":true}"#
    );
    assert_eq!(
        fs::read(claude.join(".credentials.json")).expect("read"),
        CREDS_V1
    );
    assert_eq!(
        fs::read(runtime.join(".credentials.json")).expect("read"),
        CREDS_V2
    );
}

#[test]
fn mirror_tree_skips_identical_files_with_different_mtimes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    let canonical_file = claude.join("state.json");
    let runtime_file = runtime.join("state.json");
    fs::write(&canonical_file, br#"{"same":true}"#).expect("write canonical");
    fs::write(&runtime_file, br#"{"same":true}"#).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical_file, past);
    set_mtime(&runtime_file, now);

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        canonical_file
            .metadata()
            .expect("canonical meta")
            .modified()
            .ok(),
        Some(past)
    );
    assert_eq!(
        runtime_file
            .metadata()
            .expect("runtime meta")
            .modified()
            .ok(),
        Some(now)
    );
    assert_eq!(
        fs::read(&canonical_file).expect("read canonical"),
        br#"{"same":true}"#
    );
}

#[test]
fn mirror_tree_seeds_runtime_only_file_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(runtime.join("runtime-only.json"), br#"{"who":"cc"}"#).expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("runtime-only.json")).expect("read"),
        br#"{"who":"cc"}"#
    );
    assert!(runtime.join("runtime-only.json").exists()); // runtime side preserved
}

#[test]
fn mirror_tree_seeds_canonical_only_file_to_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("user-edit.json"), br#"{"who":"user"}"#).expect("write canonical");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(runtime.join("user-edit.json")).expect("read"),
        br#"{"who":"user"}"#
    );
}

#[test]
fn mirror_tree_seeds_runtime_only_nested_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects")).expect("mkdir claude/projects");
    fs::create_dir_all(runtime.join("projects").join("new")).expect("mkdir runtime nested");
    fs::write(
        runtime.join("projects").join("new").join("state.json"),
        br#"{"step":1}"#,
    )
    .expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("projects").join("new").join("state.json")).expect("read"),
        br#"{"step":1}"#
    );
    assert!(
        runtime
            .join("projects")
            .join("new")
            .join("state.json")
            .exists()
    );
}

/// A dir `mirror_tree` seeds back onto the canonical `~/.claude/` side (the
/// runtime side created it first, e.g. CC writing a fresh session-state tree
/// under the runtime's `CLAUDE_CONFIG_DIR`) must land owner-only like every
/// other dir clauth creates under `~/.claude/`, not at the process umask
/// (typically 0755) — same invariant as the rescue path, different trigger
/// (the Fake-symlink-mode watchdog tick instead of isolated-runtime teardown).
#[cfg(unix)]
#[test]
fn mirror_tree_creates_canonical_side_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects")).expect("mkdir claude/projects");
    fs::create_dir_all(runtime.join("projects").join("new")).expect("mkdir runtime nested");
    fs::write(
        runtime.join("projects").join("new").join("state.json"),
        br#"{"step":1}"#,
    )
    .expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");

    let mode = fs::metadata(claude.join("projects").join("new"))
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o700,
        "a dir mirror_tree creates under ~/.claude must not land at the process umask"
    );
}

#[test]
fn mirror_tree_seeds_canonical_only_nested_to_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects").join("alpha")).expect("mkdir canonical nested");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(
        claude.join("projects").join("alpha").join("notes.json"),
        br#"{"note":"hi"}"#,
    )
    .expect("write canonical");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(runtime.join("projects").join("alpha").join("notes.json")).expect("read"),
        br#"{"note":"hi"}"#
    );
}

#[test]
fn copy_file_overwrites_existing_destination() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = tmp.path().join("dst.json");
    fs::write(&src, b"new bytes").expect("write src");
    fs::write(&dst, b"old bytes").expect("write dst");

    copy_file(&src, &dst).expect("copy_file");
    assert_eq!(fs::read(&dst).expect("read dst"), b"new bytes");
}

#[test]
fn copy_file_creates_missing_parent_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = tmp.path().join("nested").join("deeper").join("dst.json");
    fs::write(&src, b"payload").expect("write src");

    copy_file(&src, &dst).expect("copy_file");
    assert_eq!(fs::read(&dst).expect("read dst"), b"payload");
}

#[test]
fn copy_file_leaves_no_tmp_artifact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = tmp.path().join("dst.json");
    fs::write(&src, b"payload").expect("write src");

    copy_file(&src, &dst).expect("copy_file");

    // Any `.dst.json.tmp.*` sidecar must be renamed away after the atomic write.
    // Matched by PREFIX, not by the exact `<pid>` name: pinning the full name
    // would pass vacuously the moment the tmp scheme gains a component.
    let stray: Vec<String> = dir_entry_names(tmp.path())
        .into_iter()
        .filter(|n| n.starts_with(".dst.json.tmp."))
        .collect();
    assert!(
        stray.is_empty(),
        "atomic copy must not leave a tmp file, found {stray:?}"
    );
}

// A racing reader must never see a torn file — only old or complete-new bytes.
// This is the invariant that lets mirror_tree run lockless: rename is the
// atomicity boundary. A non-atomic copy (truncate-then-stream) would fail this.
#[test]
fn copy_file_visible_state_is_never_torn() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = Arc::new(tmp.path().join("dst.json"));

    let old = vec![b'a'; 64 * 1024];
    let new = vec![b'b'; 64 * 1024];
    fs::write(&src, &new).expect("write src");
    fs::write(dst.as_ref(), &old).expect("seed dst");

    let stop = Arc::new(AtomicBool::new(false));
    let reader_dst = dst.clone();
    let reader_stop = stop.clone();
    let old_clone = old.clone();
    let new_clone = new.clone();
    let reader = std::thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            // mid-rename: path may not resolve; any successful read must be old or complete-new
            if let Ok(bytes) = fs::read(reader_dst.as_ref()) {
                assert!(
                    bytes == old_clone || bytes == new_clone,
                    "reader observed a torn file ({} bytes)",
                    bytes.len()
                );
            }
        }
    });

    for _ in 0..200 {
        copy_file(&src, &dst).expect("copy_file");
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader panicked");
    assert_eq!(fs::read(dst.as_ref()).expect("final read"), new);
}

/// Same invariant for the BULK materialize path. Under `LinkMode::Fake` a second
/// session's `acquire` copies new `~/.claude` entries into a tree a live sibling
/// is already using, while that sibling's lockless `mirror_tree` walks it. A
/// truncate-then-stream copy is byte-different and mtime-now, so `merge_path`
/// reads it as the newer side and copies the PARTIAL bytes back over
/// `~/.claude/<entry>` — operator data loss outside the runtime tree.
#[test]
fn copy_tree_visible_state_is_never_torn() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = Arc::new(tmp.path().join("dst.json"));

    let old = vec![b'a'; 64 * 1024];
    let new = vec![b'b'; 64 * 1024];
    fs::write(&src, &new).expect("write src");
    fs::write(dst.as_ref(), &old).expect("seed dst");

    let stop = Arc::new(AtomicBool::new(false));
    let reader_dst = dst.clone();
    let reader_stop = stop.clone();
    let old_clone = old.clone();
    let new_clone = new.clone();
    let reader = std::thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            if let Ok(bytes) = fs::read(reader_dst.as_ref()) {
                assert!(
                    bytes == old_clone || bytes == new_clone,
                    "reader observed a torn file ({} bytes)",
                    bytes.len()
                );
            }
        }
    });

    for _ in 0..200 {
        copy_tree(&src, &dst).expect("copy_tree");
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader panicked");
    assert_eq!(fs::read(dst.as_ref()).expect("final read"), new);
}

/// Both fake-mode publish paths must carry the source's mode over. `~/.claude`
/// holds `statusline.sh`, hooks, and plugin executables; a copy at 0644 runs a
/// Claude Code whose statusline and hooks fail. A read-then-`atomic_write`
/// creates at the umask, which is why both paths stream through `std::fs::copy`.
///
/// The mirror leg is the one that used to lose it, and in BOTH directions: the
/// bit only dies on the first edit after the tree is built, because
/// `files_match` short-circuits identical files until then.
#[cfg(unix)]
#[test]
fn both_fake_mode_publish_paths_preserve_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;

    let mode_of = |p: &Path| {
        fs::metadata(p)
            .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
            .permissions()
            .mode()
            & 0o777
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");

    // 1. The bulk materialize walk.
    let hook = claude.join("hook.sh");
    fs::write(&hook, b"#!/bin/sh\necho v1\n").expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    copy_tree(&hook, &runtime.join("hook.sh")).expect("copy_tree");
    assert_eq!(
        mode_of(&runtime.join("hook.sh")),
        0o755,
        "a hook materialized into the runtime tree must stay executable"
    );

    // 2. The mirror leg, ~/.claude → runtime: the operator edits the hook, so
    //    `files_match` stops short-circuiting and the copy actually runs.
    fs::write(&hook, b"#!/bin/sh\necho v2\n").expect("edit hook");
    set_mtime(&hook, SystemTime::now() + Duration::from_secs(60));
    mirror_tree(&claude, &runtime).expect("mirror to runtime");
    assert_eq!(
        fs::read(runtime.join("hook.sh")).expect("read runtime hook"),
        b"#!/bin/sh\necho v2\n",
        "the edit must actually reach the runtime — otherwise the mode assert is vacuous"
    );
    assert_eq!(
        mode_of(&runtime.join("hook.sh")),
        0o755,
        "the mirror must not strip +x off the runtime copy"
    );

    // 3. The mirror leg, runtime → ~/.claude: a write-back at 0644 would strip
    //    +x off the operator's own file, outside the runtime tree.
    let back = runtime.join("cc-made-this.sh");
    fs::write(&back, b"#!/bin/sh\necho cc\n").expect("write runtime-only hook");
    fs::set_permissions(&back, fs::Permissions::from_mode(0o755)).expect("chmod runtime hook");
    mirror_tree(&claude, &runtime).expect("mirror to claude");
    assert_eq!(
        mode_of(&claude.join("cc-made-this.sh")),
        0o755,
        "the write-back must not strip +x off the operator's side"
    );
}

#[test]
fn detect_link_mode_returns_real_on_unix() {
    // Same lock every `with_link_mode` test holds, so a parallel override can
    // never leak into the probe this test exists to check.
    let _lock = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let mode = detect_link_mode(tmp.path()).expect("detect");
    #[cfg(unix)] // Unix CI always grants symlinks; Windows depends on dev mode
    assert_eq!(mode, LinkMode::Real);
    #[cfg(not(unix))]
    let _ = mode;
}

// ── HOME-mutating tests ────────────────────────────────────────────────────────

/// Redirect `home_dir()` into `root` for the duration of `f`, serialized on
/// `profile::HOME_TEST_LOCK`. Uses the process-global `HOME_OVERRIDE` rather
/// than `$HOME` so resolution matches on Windows too, where `dirs::home_dir()`
/// reads `USERPROFILE`, not `HOME`. The override is cleared on drop so a
/// panicking test can't leak it into the next test.
fn with_fake_home<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    let _lock = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            crate::profile::clear_home_override();
        }
    }
    crate::profile::set_home_override(root.to_path_buf());
    let _clear = ClearOnDrop;
    f()
}

/// Force [`detect_link_mode`] to report `mode` for the duration of `f`.
/// `try_real_symlink` always succeeds on unix, so the fake-symlink transport —
/// and the shared bare-stem tree it selects — is otherwise unreachable from a
/// Linux/macOS run. Call INSIDE [`with_fake_home`]: its `HOME_TEST_LOCK` hold is
/// what serializes this process-global override.
fn with_link_mode<T>(mode: LinkMode, f: impl FnOnce() -> T) -> T {
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            clear_link_mode_override();
        }
    }
    set_link_mode_override(mode);
    let _clear = ClearOnDrop;
    f()
}

/// Build `~/.claude/` (required by `acquire`).
fn fake_claude_home(root: &Path) -> PathBuf {
    let claude = root.join(".claude");
    fs::create_dir_all(&claude).expect("mkdir .claude");
    claude
}

fn make_profile(name: &str) -> crate::profile::Profile {
    crate::profile::Profile::new(name.to_string(), None, None)
}

/// The `<sid>` keying a live session's dirs, read back off its runtime path.
fn sid_of(runtime: &Path) -> String {
    runtime
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("runtime-"))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("{} is not a per-session runtime dir", runtime.display()))
}

/// A live session's id, read back off its own marker dir — which holds exactly
/// one marker, named for the session. Flavor-agnostic, unlike [`sid_of`].
fn live_sid(rt: &ProfileRuntime) -> String {
    let mut names = dir_entry_names(rt.sessions_dir());
    assert_eq!(names.len(), 1, "a marker dir holds exactly one marker");
    names.remove(0)
}

/// Sorted file names directly under `dir`.
fn dir_entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

#[test]
fn build_runtime_dir_writes_settings_not_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            claude_home.join("settings.json"),
            br#"{"env":{"EXISTING":"1"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let settings_dst = runtime.join("settings.json");
        let meta = settings_dst.symlink_metadata().expect("settings present");
        assert!(
            !meta.file_type().is_symlink(),
            "settings.json must not be a symlink"
        );

        let expected =
            build_claude_settings_json(Some(&claude_home.join("settings.json")), &profile, &[])
                .expect("build_claude_settings_json");
        let actual = fs::read_to_string(&settings_dst).expect("read settings");
        assert_eq!(actual, expected);
    });
}

#[test]
fn build_runtime_dir_strips_active_env_from_another_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // Live settings carry the active profile's custom env (`FOO`) plus an
        // operator-owned key (`KEEP`) that must survive every switch/start.
        fs::write(
            claude_home.join("settings.json"),
            br#"{"env":{"FOO":"active","KEEP":"mine"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let target = make_profile("target");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir_with_active_env(
            &runtime,
            &claude_home,
            &target,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
            &["FOO".to_string()],
        )
        .expect("build");

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(runtime.join("settings.json")).expect("read"))
                .expect("parse");
        assert!(
            settings["env"].get("FOO").is_none(),
            "active profile's custom env must not leak into another profile's runtime"
        );
        assert_eq!(
            settings["env"]["KEEP"],
            serde_json::json!("mine"),
            "operator env inherited untouched"
        );
    });
}

#[test]
fn build_runtime_dir_active_env_strip_is_noop_when_target_is_active() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            claude_home.join("settings.json"),
            br#"{"env":{"FOO":"active"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let mut target = make_profile("target");
        target.env.insert("FOO".into(), "active".into());
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir_with_active_env(
            &runtime,
            &claude_home,
            &target,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
            &["FOO".to_string()],
        )
        .expect("build");

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(runtime.join("settings.json")).expect("read"))
                .expect("parse");
        assert_eq!(
            settings["env"]["FOO"],
            serde_json::json!("active"),
            "starting the active profile itself keeps its own env (strip is a no-op)"
        );
    });
}

#[test]
fn build_runtime_dir_credentials_not_from_claude_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // ~/.claude/.credentials.json must NOT appear in runtime
        fs::write(claude_home.join(".credentials.json"), CREDS_V1).expect("write creds");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json"); // no canonical → runtime creds absent

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let runtime_creds = runtime.join(".credentials.json");
        assert!(
            !runtime_creds.exists(),
            ".credentials.json from ~/.claude/ must not be copied into runtime"
        );
    });
}

#[test]
fn build_runtime_dir_fake_preserves_live_runtime_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, CREDS_V2).expect("write runtime credentials");
        let past = SystemTime::now() - Duration::from_secs(60);
        let now = SystemTime::now();
        set_mtime(&canonical, past);
        set_mtime(&runtime_creds, now);

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
        assert_eq!(fs::read(&runtime_creds).expect("read runtime"), CREDS_V2);
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_real_preserves_live_runtime_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, CREDS_V2).expect("write runtime credentials");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
        assert!(
            runtime_creds
                .symlink_metadata()
                .expect("runtime credentials meta")
                .file_type()
                .is_symlink()
        );
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_real_keeps_invalid_runtime_credentials_for_retry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, b"partial write").expect("write runtime credentials");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V1);
        assert_eq!(
            fs::read(&runtime_creds).expect("read runtime"),
            b"partial write"
        );
    });
}

#[test]
fn build_runtime_dir_other_entries_materialized() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // A few ordinary entries that should be mirrored.
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::write(claude_home.join("history.jsonl"), b"{}").expect("write history");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        assert!(runtime.join("projects").is_dir(), "projects dir copied"); // Fake mode: copied, not symlinked
        assert!(
            runtime.join("history.jsonl").exists(),
            "history.jsonl copied"
        );
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_other_entries_symlinked_on_unix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("todos.json"), b"[]").expect("write todos");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        let dst = runtime.join("todos.json");
        assert!(
            dst.symlink_metadata()
                .expect("todos present")
                .file_type()
                .is_symlink(),
            "todos.json should be a symlink in Real mode"
        );
    });
}

#[test]
fn build_runtime_dir_links_claude_json_from_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // ~/.claude.json sits next to ~/.claude/, not inside it
        fs::write(tmp.path().join(".claude.json"), br#"{"userId":"u1"}"#)
            .expect("write .claude.json");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let dst = runtime.join(".claude.json");
        assert!(dst.exists(), ".claude.json must appear in runtime");
        assert_eq!(
            fs::read(&dst).expect("read"),
            br#"{"userId":"u1"}"#,
            "content must match source"
        );
    });
}

/// `runtime/settings.json` carries clauth-owned credential routing for an
/// api-key profile (top-level `apiKeyHelper` naming the profile, plus the
/// base_url and model env keys), so it is a credential file and must land
/// 0o600 like every other clauth-owned write. The raw key is NOT in this file
/// (it lives in `config.toml`, minted per request by the helper) — but the
/// helper string and the surrounding env are still operator-sensitive, so the
/// perm invariant is unchanged from the pre-helper era. The seeded
/// `.claude.json` rides the same rule.
#[cfg(unix)]
#[test]
fn runtime_settings_and_seed_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("settings.json"), br#"{}"#).expect("write settings");
        fs::write(tmp.path().join(".claude.json"), br#"{"numStartups":1}"#).expect("write global");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = crate::profile::Profile::new(
            "keyed".to_string(),
            Some("https://api.example.com".to_string()),
            Some("sk-secret-key".to_string()),
        );

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &tmp.path().join("creds.json"),
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let settings = runtime.join("settings.json");
        let settings_bytes = fs::read_to_string(&settings).expect("read settings");
        assert!(
            settings_bytes.contains("apiKeyHelper"),
            "precondition: the apiKeyHelper wiring is in this file (got: {settings_bytes})"
        );
        assert!(
            !settings_bytes.contains("sk-secret-key"),
            "the raw api key must NOT be in this file — only the helper command string"
        );
        let mode = fs::metadata(&settings).expect("meta").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "runtime settings.json holds the api key; mode should be 0o600, got {:#o}",
            mode & 0o777,
        );
        let seed_mode = fs::metadata(runtime.join(".claude.json"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(
            seed_mode & 0o777,
            0o600,
            "seeded .claude.json mode should be 0o600, got {:#o}",
            seed_mode & 0o777,
        );
    });
}

/// A settings.json an older build left at 0o644 keeps its bytes forever once
/// the profile stops changing, so a byte-only write gate would never retighten
/// it. The gate has to see the mode too.
#[cfg(unix)]
#[test]
fn runtime_settings_retightens_a_loose_file_with_current_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("settings.json"), br#"{}"#).expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = crate::profile::Profile::new(
            "keyed".to_string(),
            Some("https://api.example.com".to_string()),
            Some("sk-secret-key".to_string()),
        );

        // Byte-identical to what the merge produces, at the old umask mode.
        let current =
            build_claude_settings_json(Some(&claude_home.join("settings.json")), &profile, &[])
                .expect("build_claude_settings_json");
        let settings = runtime.join("settings.json");
        fs::write(&settings, &current).expect("write legacy settings");
        fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).expect("chmod");

        write_merged_settings(&runtime, &claude_home, &profile, Isolation::Shared, &[])
            .expect("write_merged_settings");

        let mode = fs::metadata(&settings).expect("meta").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a 0o644 settings.json from an older build must be retightened, got {:#o}",
            mode & 0o777,
        );
        assert_eq!(
            fs::read_to_string(&settings).expect("read"),
            current,
            "content must be unchanged by the mode repair"
        );
    });
}

/// Issue #17 systemic finding: a raw copy is born carrying whichever account
/// was active at seed time, wrong for every non-active profile. Seeding must
/// strip it so the fresh runtime starts identity-less and Claude Code
/// re-derives it from THIS profile's own credentials.
#[test]
fn seed_claude_json_strips_oauth_account_from_fresh_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            tmp.path().join(".claude.json"),
            br#"{"oauthAccount":{"emailAddress":"active@x"},"numStartups":4}"#,
        )
        .expect("write global .claude.json");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");

        seed_claude_json(&runtime, &claude_home).expect("seed");

        let dst = runtime.join(".claude.json");
        let seeded: serde_json::Value =
            serde_json::from_slice(&fs::read(&dst).expect("read seeded")).expect("parse");
        assert!(
            seeded.get("oauthAccount").is_none(),
            "a freshly seeded runtime copy must not inherit the active profile's identity"
        );
        assert_eq!(seeded["numStartups"], serde_json::json!(4));
    });
}

/// A profile whose runtime already has its own real `.claude.json` (its own
/// prior login wrote a genuine identity) must keep it — seeding only applies
/// to a missing file or a leftover shared symlink, never to an existing copy.
#[test]
fn seed_claude_json_leaves_existing_real_copy_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            tmp.path().join(".claude.json"),
            br#"{"oauthAccount":{"emailAddress":"active@x"},"numStartups":4}"#,
        )
        .expect("write global .claude.json");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let dst = runtime.join(".claude.json");
        let own: &[u8] = br#"{"oauthAccount":{"emailAddress":"own@x"},"numStartups":1}"#;
        fs::write(&dst, own).expect("write existing runtime copy");

        seed_claude_json(&runtime, &claude_home).expect("seed");

        assert_eq!(
            fs::read(&dst).expect("read"),
            own,
            "an existing real copy already has its own identity and must not be reseeded"
        );
    });
}

#[test]
fn has_live_session_false_when_no_sessions_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        assert!(!has_live_session("ghost")); // no sessions dir → false, not error
    });
}

#[test]
fn has_live_session_false_when_sessions_dir_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("empty")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        assert!(!has_live_session("empty"));
    });
}

#[test]
fn has_live_session_false_when_all_sessions_dead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("dead")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("99999"), b"").expect("write dead pid"); // unlocked file = dead
        assert!(!has_live_session("dead"));
    });
}

#[test]
fn has_live_session_true_when_any_session_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("alive")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        let pid_path = sessions.join("12345");
        let file = open_pid_file(&pid_path).expect("open pid");
        file.lock().expect("lock pid");
        assert!(has_live_session("alive"));
        drop(file);
        // The probe is deliberately fail-alive (any try_lock I/O error reads
        // as "alive" — see `is_session_alive`), so one transient error under a
        // parallel suite run can inflate a single reading. Poll briefly: only
        // a PERSISTENTLY-alive reading is a regression. Same hardening as
        // `live_session_count_counts_only_alive`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let settled_dead = loop {
            let alive = has_live_session("alive");
            if !alive {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(settled_dead, "a dropped session lock must read as dead");
    });
}

#[test]
fn has_live_session_true_with_mixed_alive_and_dead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("mixed")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("11111"), b"").expect("write dead pid"); // dead
        let live_path = sessions.join("22222"); // live
        let file = open_pid_file(&live_path).expect("open live pid");
        file.lock().expect("lock live pid");
        assert!(has_live_session("mixed"));
        drop(file);
    });
}

#[test]
fn live_session_count_counts_only_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("counted")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("11111"), b"").expect("write dead pid"); // dead
        let a = open_pid_file(&sessions.join("22222")).expect("open a");
        a.lock().expect("lock a");
        let b = open_pid_file(&sessions.join("33333")).expect("open b");
        b.lock().expect("lock b");
        // The probe is deliberately fail-alive (any try_lock I/O error reads
        // as "alive" — see `is_session_alive`), so one transient error under a
        // parallel suite run can inflate a single reading. Poll briefly: only
        // a PERSISTENT wrong count is a regression.
        let settled = |expect: usize| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let n = live_session_count("counted");
                if n == expect || std::time::Instant::now() >= deadline {
                    return n;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        assert_eq!(settled(2), 2);
        drop(a);
        assert_eq!(settled(1), 1);
        assert_eq!(live_session_count("ghost"), 0); // no sessions dir → zero
    });
}

#[test]
fn acquire_creates_runtime_and_pid_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("lifecycle");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");

        assert!(
            rt.config_dir().is_dir(),
            "runtime dir must exist after acquire"
        );

        let sessions = rt.sessions_dir().to_path_buf();
        let sid = sid_of(rt.config_dir());
        assert_eq!(
            dir_entry_names(&sessions),
            vec![sid.clone()],
            "exactly one marker, named for this session"
        );
        let pid_file = sessions.join(&sid);
        assert!(
            sid.starts_with(&format!("{}-", std::process::id())),
            "the session id must carry the `<pid>-` prefix, got {sid}"
        );
        assert!(
            is_session_alive(&pid_file),
            "PID file must be flock-held while runtime is alive"
        );

        let profile_dir = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("lifecycle");
        let expected_runtime = profile_dir.join(format!("runtime-{sid}"));
        assert_eq!(rt.config_dir(), expected_runtime);
        assert_eq!(sessions, profile_dir.join(format!("sessions-{sid}")));

        assert!(
            rt.config_dir().join("settings.json").exists(),
            "settings.json must be written"
        );

        drop(rt);

        assert!(
            !expected_runtime.exists(),
            "runtime dir torn down on last-session drop"
        );
        assert!(
            !sessions.exists(),
            "sessions dir removed when no live siblings remain"
        );
    });
}

/// Black-box `clauth start` isolation: a full `acquire` must build the runtime
/// tree from the profile's OWN canonical credentials and never leak the live
/// `~/.claude/.credentials.json` (a different account's tokens) into it. Also
/// pins that `acquire` leaves the real home's credential file untouched.
#[test]
fn acquire_isolates_credentials_from_real_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // The real `~/.claude/.credentials.json` belongs to a DIFFERENT account
        // (a "wrong" chain). Isolation means it must never reach the runtime.
        let live_creds = claude_home.join(".credentials.json");
        fs::write(&live_creds, CREDS_V1).expect("write live creds");

        // Pre-stage the profile's own canonical credentials (what `clauth start`
        // restores for this profile) with a DISTINCT token chain.
        let profile = make_profile("isolated");
        let canonical = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("isolated")
            .join("credentials.json");
        fs::create_dir_all(canonical.parent().expect("canonical parent"))
            .expect("mkdir profile dir");
        fs::write(&canonical, CREDS_V2).expect("write canonical");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        let runtime_creds = rt.config_dir().join(".credentials.json");

        // The runtime's credentials resolve to the profile's OWN chain (V2),
        // not the live wrong-account chain (V1). On Unix this is a symlink into
        // canonical; either way the resolved bytes must be the profile's.
        assert_eq!(
            fs::read(&runtime_creds).expect("read runtime creds"),
            CREDS_V2,
            "runtime must carry the profile's canonical chain, not the live one"
        );
        assert_ne!(
            fs::read(&runtime_creds).expect("read runtime creds"),
            CREDS_V1,
            "the live ~/.claude chain must never leak into the runtime"
        );

        // The real home's credential file is untouched by the launch.
        assert_eq!(
            fs::read(&live_creds).expect("read live creds"),
            CREDS_V1,
            "acquire must not overwrite the real ~/.claude/.credentials.json"
        );

        // settings.json is a per-profile rewrite, never a symlink into the
        // shared home — the isolation boundary for env/base-url too.
        let settings = rt.config_dir().join("settings.json");
        assert!(
            !settings
                .symlink_metadata()
                .expect("settings present")
                .file_type()
                .is_symlink(),
            "runtime settings.json must be a per-profile copy, not a shared symlink"
        );

        drop(rt);
    });
}

/// Regression: one process holding two concurrent sessions of the same
/// profile+flavor must not collide on the session file. Before the per-acquire
/// `-<n>` suffix both keyed `sessions/<pid>`, so the second `acquire` blocked
/// forever on the first's `flock(2)` — the background-`delegate` hang where a
/// second same-profile job never spawned a session. Both must register live.
#[test]
fn acquire_twice_same_process_counts_two_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("concurrent");

        let rt1 = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        // Pre-fix this second acquire blocks forever on the shared PID flock.
        let rt2 = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        assert_eq!(
            live_session_count("concurrent"),
            2,
            "two concurrent same-process sessions must both register live"
        );

        let rt1_runtime = rt1.config_dir().to_path_buf();

        drop(rt2);
        assert!(
            rt1_runtime.is_dir(),
            "the surviving session's runtime is untouched by a sibling's teardown"
        );
        assert_eq!(live_session_count("concurrent"), 1);

        drop(rt1);
        assert!(
            !rt1_runtime.exists(),
            "runtime torn down once its own session drops"
        );
    });
}

/// Every `clauth start` session gets its OWN tree — the shared flavor included,
/// which two same-profile sessions used to share. Pins the exact per-session
/// names and the `runtime<rest>` ↔ `sessions<rest>` pairing they rest on.
#[test]
fn two_shared_sessions_get_independent_trees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("twin");

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        assert_ne!(
            a.config_dir(),
            b.config_dir(),
            "two shared sessions of one profile must not share a runtime tree"
        );
        assert_ne!(
            a.sessions_dir(),
            b.sessions_dir(),
            "two shared sessions of one profile must not share a marker dir"
        );

        let profile_dir = tmp.path().join(".clauth").join("profiles").join("twin");
        for rt in [&a, &b] {
            let sid = sid_of(rt.config_dir());
            assert_eq!(rt.config_dir(), profile_dir.join(format!("runtime-{sid}")));
            assert_eq!(
                rt.sessions_dir(),
                profile_dir.join(format!("sessions-{sid}"))
            );
            assert_eq!(
                dir_entry_names(rt.sessions_dir()),
                vec![sid],
                "a marker dir holds this session's marker and no other's"
            );
            assert!(
                rt.config_dir().join("settings.json").is_file(),
                "each tree is built independently, not shared"
            );
        }
        assert_eq!(live_session_count("twin"), 2);

        drop(b);
        drop(a);
    });
}

/// THE UPGRADE GATE. A clauth process built before the per-session layout probes
/// exactly `<profile>/sessions[-isolated]`. Without a marker there its
/// `has_live_session` reads a live new-layout session as idle. That old binary
/// still gates rotation on liveness, so it would spend the single-use refresh
/// token the session holds — costing that session one failed refresh, not the
/// account. Post-upgrade the old binary is the DEFAULT supervisor until the next
/// restart (`clauth daemon --replace` exists for exactly that).
///
/// The `live_sessions_at` assertion below IS the old binary's predicate, applied
/// to the old binary's path.
#[test]
fn acquire_stamps_the_pre_upgrade_liveness_marker_for_both_flavors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());

        for (name, isolation, legacy_dir) in [
            ("upgrade-shared", Isolation::Shared, "sessions"),
            ("upgrade-iso", Isolation::Isolated, "sessions-isolated"),
        ] {
            let profile = make_profile(name);
            let rt = ProfileRuntime::acquire(&profile, isolation, &[], false).expect("acquire");
            let sid = live_sid(&rt);

            let legacy = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join(name)
                .join(legacy_dir);
            let legacy_marker = legacy.join(&sid);
            assert!(
                legacy_marker.is_file(),
                "no upgrade-compat marker at {}",
                legacy_marker.display()
            );
            assert!(
                is_session_alive(&legacy_marker),
                "the upgrade-compat marker must be flock-held for the session's life"
            );
            assert_eq!(
                live_sessions_at(&legacy),
                Some(1),
                "a pre-upgrade clauth probes exactly {legacy_dir} and must see this session"
            );
            assert_eq!(
                live_session_count(name),
                1,
                "the compat marker and the per-session marker are ONE session, not two"
            );

            drop(rt);

            assert!(
                !legacy_marker.exists(),
                "teardown must drop the upgrade-compat marker"
            );
            assert!(
                !legacy.exists(),
                "the last session out removes the shared compat dir"
            );
            assert_eq!(live_session_count(name), 0);
        }
    });
}

/// `stamp_legacy_marker` must decline rather than block when the marker is
/// already held, and leave the file exactly as it found it.
#[test]
fn stamp_legacy_marker_declines_a_marker_another_holder_owns() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("sessions").join("4242-0");
    fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir sessions");
    let held = open_pid_file(&marker).expect("open marker");
    held.lock().expect("lock marker");

    assert!(
        stamp_legacy_marker(&marker).is_none(),
        "a marker another holder owns must not be adopted"
    );
    assert!(marker.is_file(), "declining must not disturb the file");
    assert!(
        is_session_alive(&marker),
        "the holder's flock must survive the decline"
    );

    drop(held);
    assert!(
        stamp_legacy_marker(&marker).is_some(),
        "an unlocked marker is free to take"
    );
}

/// Teardown must not unlink a marker this session never owned. `stamp_legacy_marker`
/// yields `None` when `try_lock` loses to a live process that minted the same sid,
/// and unlinking on that path deletes a FOREIGN session's liveness signal — the
/// same rotation burn the compat marker exists to prevent.
#[test]
fn teardown_leaves_a_pre_upgrade_marker_it_never_owned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());

        // `acquire` mints exactly one `SessionId`, and `with_fake_home` holds the
        // lock that is the only way into `acquire`, so the counter cannot move
        // between this probe and the acquire below. The assert after the acquire
        // is what catches that arithmetic going stale.
        let probe = SessionId::mint();
        let (pid, seq) = probe.as_str().split_once('-').expect("<pid>-<seq>");
        let foreign_sid = format!("{pid}-{}", seq.parse::<u64>().expect("seq") + 1);

        let legacy = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("foreign")
            .join("sessions");
        fs::create_dir_all(&legacy).expect("mkdir legacy sessions");
        let foreign_marker = legacy.join(&foreign_sid);
        let held = open_pid_file(&foreign_marker).expect("open foreign marker");
        held.lock().expect("lock foreign marker");

        let profile = make_profile("foreign");
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        assert_eq!(
            live_sid(&rt),
            foreign_sid,
            "sid arithmetic drifted — `acquire` no longer mints exactly one id, \
             so this test is no longer posing the collision it claims to"
        );

        drop(rt);

        assert!(
            foreign_marker.is_file(),
            "teardown unlinked a liveness marker owned by another live process"
        );
        assert!(
            is_session_alive(&foreign_marker),
            "the foreign holder's flock must be untouched"
        );
        drop(held);
    });
}

/// Two same-profile sessions share the one compat dir, so it may only go when
/// the last of them releases.
#[test]
fn the_pre_upgrade_marker_dir_survives_until_the_last_session_leaves() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("upgrade-twin");
        let legacy = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("upgrade-twin")
            .join("sessions");

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        assert_eq!(
            live_sessions_at(&legacy),
            Some(2),
            "both sessions must be visible to a pre-upgrade probe"
        );
        assert_eq!(
            live_session_count("upgrade-twin"),
            2,
            "two sessions, four markers, still two sessions"
        );

        drop(b);
        assert_eq!(live_sessions_at(&legacy), Some(1));
        assert!(
            legacy.is_dir(),
            "the compat dir is shared — it must survive"
        );
        assert_eq!(live_session_count("upgrade-twin"), 1);

        drop(a);
        assert!(!legacy.exists());
    });
}

/// Teardown is per session: dropping one of two same-profile shared sessions
/// discards only its own tree and marker.
#[test]
fn dropping_one_shared_session_leaves_the_sibling_intact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("survivor");

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        let a_runtime = a.config_dir().to_path_buf();
        let a_sessions = a.sessions_dir().to_path_buf();
        let a_marker = a_sessions.join(sid_of(&a_runtime));
        let b_runtime = b.config_dir().to_path_buf();
        let b_sessions = b.sessions_dir().to_path_buf();
        fs::write(a_runtime.join("survivor.txt"), b"keep me").expect("seed a's tree");

        drop(b);

        assert!(
            !b_runtime.exists(),
            "the dropped session's tree is discarded"
        );
        assert!(
            !b_sessions.exists(),
            "the dropped session's marker dir goes with it"
        );
        assert!(a_runtime.is_dir(), "the sibling's tree must survive");
        assert_eq!(
            fs::read(a_runtime.join("survivor.txt")).expect("read sibling file"),
            b"keep me",
            "the sibling's tree contents must be untouched"
        );
        assert_eq!(dir_entry_names(&a_sessions), vec![sid_of(&a_runtime)]);
        assert!(
            is_session_alive(&a_marker),
            "the sibling's marker must still be flock-held"
        );
        assert_eq!(live_session_count("survivor"), 1);

        drop(a);
        assert!(!a_runtime.exists());
        assert!(!a_sessions.exists());
    });
}

// ── LinkMode::Fake keeps the shared (profile, flavor) tree ────────────────────

/// The naming rule as a unit. `LinkMode::Real` keys each session's pair by its
/// own `<sid>`; `LinkMode::Fake` returns the bare stem every session of that
/// profile+flavor shares. In all four cases the two names must satisfy the
/// module's one layout rule (`runtime<rest>` ↔ `sessions<rest>`) and both strict
/// predicates, so no enumeration can miss a dir the naming produced.
#[test]
fn paired_dir_names_key_on_link_mode() {
    let sid = "4242-7";
    let cases = [
        (Isolation::Shared, LinkMode::Fake, "runtime", "sessions"),
        (
            Isolation::Isolated,
            LinkMode::Fake,
            "runtime-isolated",
            "sessions-isolated",
        ),
        (
            Isolation::Shared,
            LinkMode::Real,
            "runtime-4242-7",
            "sessions-4242-7",
        ),
        (
            Isolation::Isolated,
            LinkMode::Real,
            "runtime-isolated-4242-7",
            "sessions-isolated-4242-7",
        ),
    ];
    for (isolation, mode, want_runtime, want_sessions) in cases {
        let (runtime, sessions) = paired_dir_names(isolation, sid, mode);
        assert_eq!(runtime, want_runtime, "{isolation:?}/{mode:?} runtime name");
        assert_eq!(
            sessions, want_sessions,
            "{isolation:?}/{mode:?} sessions name"
        );
        assert_eq!(
            paired_sessions_name(&runtime).as_deref(),
            Some(sessions.as_str()),
            "{runtime} must pair with {sessions}"
        );
        assert_eq!(
            paired_runtime_name(&sessions).as_deref(),
            Some(runtime.as_str()),
            "{sessions} must pair back to {runtime}"
        );
        assert!(is_runtime_dir_name(&runtime), "GC must reach {runtime}");
        assert!(is_sessions_dir_name(&sessions), "GC must reach {sessions}");
        assert_eq!(
            is_shared_runtime_dir_name(&runtime),
            isolation == Isolation::Shared,
            "{runtime} flavor must be readable off the name alone"
        );
    }
}

/// Under `LinkMode::Fake` the tree is a recursive COPY of `~/.claude/`, so two
/// shared sessions of one profile land on ONE bare-stem tree. The real-symlink
/// counterpart — where they must NOT — is
/// `two_shared_sessions_get_independent_trees`.
#[test]
fn fake_mode_shares_one_tree_across_two_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = make_profile("faketwin");

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("first acquire");
            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("second acquire");

            let profile_dir = tmp.path().join(".clauth").join("profiles").join("faketwin");
            assert_eq!(a.config_dir(), profile_dir.join("runtime"));
            assert_eq!(b.config_dir(), profile_dir.join("runtime"));
            assert_eq!(a.sessions_dir(), profile_dir.join("sessions"));
            assert_eq!(b.sessions_dir(), profile_dir.join("sessions"));

            let mut want = vec![
                a.swap.session.as_str().to_string(),
                b.swap.session.as_str().to_string(),
            ];
            want.sort();
            assert_ne!(want[0], want[1], "the two sessions must still be distinct");
            assert_eq!(
                dir_entry_names(a.sessions_dir()),
                want,
                "one shared marker dir carrying both sessions' markers"
            );

            drop(b);
            drop(a);
        });
    });
}

/// Session 2 must neither wipe nor rebuild the tree session 1 is using — that is
/// the whole point of sharing it. The sentinel exists ONLY in the runtime tree,
/// so nothing can re-materialize it: if the second acquire wiped the tree, it is
/// gone for good.
#[test]
fn fake_mode_second_session_does_not_rebuild_the_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile = make_profile("fakecopy");

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("first acquire");

            let sentinel = "session-one-was-here.txt";
            assert!(
                !claude_home.join(sentinel).exists(),
                "the sentinel must be absent from ~/.claude, or a rebuild would restore it \
                 and this test would prove nothing"
            );
            fs::write(a.config_dir().join(sentinel), b"do not re-copy me").expect("seed sentinel");

            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("second acquire");

            assert_eq!(
                b.config_dir(),
                a.config_dir(),
                "the second session must reuse the first's tree, not pay a second copy"
            );
            assert_eq!(
                fs::read(a.config_dir().join(sentinel)).expect("read sentinel"),
                b"do not re-copy me",
                "the second acquire wiped or rebuilt a tree a live sibling is using"
            );

            drop(b);
            drop(a);
        });
    });
}

/// Liveness over a shared marker dir. A `has_live_session` false negative lets
/// a delete or disable through against a running session, so the count must
/// stay per SESSION even though two sessions share one dir — and the tree may
/// only be discarded by the last one out.
#[test]
fn fake_mode_liveness_counts_both_shared_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = make_profile("fakegate");

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("first acquire");
            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("second acquire");
            let tree = a.config_dir().to_path_buf();
            let markers = a.sessions_dir().to_path_buf();

            assert!(has_live_session("fakegate"));
            assert_eq!(live_session_count("fakegate"), 2);
            assert_eq!(
                dir_entry_names(&markers).len(),
                2,
                "one shared marker dir must carry a marker per session"
            );

            drop(b);
            assert!(has_live_session("fakegate"));
            assert_eq!(live_session_count("fakegate"), 1);
            assert!(
                tree.is_dir(),
                "the shared tree must survive while a sibling still holds it"
            );

            drop(a);
            assert!(!has_live_session("fakegate"));
            assert_eq!(live_session_count("fakegate"), 0);
            assert!(!tree.exists(), "the last session out discards the tree");
            assert!(!markers.exists());
        });
    });
}

/// A registry row carries the profile, the flavor, and the session id — but NOT
/// the transport. Probing only the per-session marker path drops every fake-mode
/// row the first time any sweep runs, while its session is live.
#[test]
fn fake_mode_registry_row_survives_gc() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = make_profile("fakerow");

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
            let sid = rt.swap.session.as_str().to_string();

            gc_stale_runtimes();

            let left: Vec<String> = crate::live_sessions::list()
                .into_iter()
                .map(|r| r.session_id)
                .collect();
            assert_eq!(
                left,
                vec![sid],
                "GC reaped a LIVE fake-mode session's registry row"
            );
            assert!(
                rt.config_dir().is_dir(),
                "GC must spare the live shared tree too"
            );

            drop(rt);
        });
    });
}

/// Under `LinkMode::Fake` the session's own marker ALREADY sits at the
/// pre-per-session path a pre-layout clauth probes, so there is no second marker
/// to stamp. Stamping one anyway would `try_lock` that same path against this
/// process's own fd, fail, and log "not lockable" on every fake-mode start. The
/// absence is structural: `legacy_marker` is `None`, so the stamp is never
/// reached.
#[test]
fn fake_mode_stamps_no_second_compat_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());

            for (name, isolation, legacy_dir) in [
                ("fakecompat-shared", Isolation::Shared, "sessions"),
                ("fakecompat-iso", Isolation::Isolated, "sessions-isolated"),
            ] {
                let profile = make_profile(name);
                let rt = ProfileRuntime::acquire(&profile, isolation, &[], false).expect("acquire");

                assert_eq!(
                    rt.legacy_marker, None,
                    "{name}: a shared-tree session's own marker IS the compat marker"
                );
                assert!(
                    rt.legacy_lock.is_none(),
                    "{name}: nothing to lock when nothing is stamped"
                );

                let legacy = tmp
                    .path()
                    .join(".clauth")
                    .join("profiles")
                    .join(name)
                    .join(legacy_dir);
                assert_eq!(
                    rt.sessions_dir(),
                    legacy,
                    "{name}: the session's marker dir must BE the pre-upgrade path"
                );
                assert_eq!(
                    live_sessions_at(&legacy),
                    Some(1),
                    "{name}: a pre-upgrade clauth probes exactly {legacy_dir} and must see this session"
                );
                assert_eq!(
                    live_session_count(name),
                    1,
                    "{name}: one marker, one session"
                );

                drop(rt);

                assert!(!legacy.exists(), "{name}: the last session out removes it");
                assert_eq!(live_session_count(name), 0);
            }
        });
    });
}

/// `build_runtime_dir` re-walk must pick up entries added between two acquires.
/// Drives `build_runtime_dir` directly to isolate the re-walk from the rest of
/// the acquire path (watchdog spawn, flock, teardown).
#[test]
fn build_runtime_dir_rewalk_picks_up_late_entries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("existing.txt"), b"v1").expect("write existing");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("rewalk");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("first build");
        assert!(
            runtime.join("existing.txt").exists(),
            "first build: existing.txt present"
        );

        fs::write(claude_home.join("late_entry.txt"), b"new").expect("write late entry");

        // second build (second session's acquire) — late entry must appear
        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("second build");
        assert!(
            runtime.join("late_entry.txt").exists(),
            "second build must pick up late_entry.txt"
        );
        assert!(
            // re-walk is additive, not destructive
            runtime.join("existing.txt").exists(),
            "second build must preserve existing.txt"
        );
    });
}

/// A second live session must prevent teardown. Drives `prune_stale_sessions`
/// on hand-placed flock files to test the count logic in isolation.
#[test]
fn prune_with_two_live_sessions_returns_two() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions");
    fs::create_dir_all(&sessions).expect("mkdir sessions");

    let pid1 = sessions.join("100001");
    let pid2 = sessions.join("100002");
    let f1 = open_pid_file(&pid1).expect("open pid1");
    f1.lock().expect("lock pid1");
    let f2 = open_pid_file(&pid2).expect("open pid2");
    f2.lock().expect("lock pid2");

    let count = prune_stale_sessions(&sessions).expect("prune");
    assert_eq!(count, 2, "both live sessions must be counted");

    drop(f2);
    let count = prune_stale_sessions(&sessions).expect("prune after drop f2");
    assert_eq!(count, 1, "one live session after f2 dropped");
    assert!(!pid2.exists(), "dead session file removed");

    drop(f1);
    let count = prune_stale_sessions(&sessions).expect("prune after drop f1");
    assert_eq!(count, 0, "no live sessions after both dropped");
    assert!(!pid1.exists(), "dead session file removed");
}

// ── sync_credentials_unlocked concurrent contention (Unix) ───────────────────
//
// Two barrier-synchronized threads call sync on the same link_path (same
// PID-suffixed tmp). Regardless of which wins the rename race, end state must
// be consistent: link_path is a symlink, canonical holds the right bytes, no
// dangling tmp.

#[cfg(unix)]
#[test]
fn sync_credentials_unlocked_concurrent_same_link_consistent_end_state() {
    use std::sync::{Arc, Barrier};

    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = Arc::new(tmp.path().join("canonical.json"));
    let link_path = Arc::new(tmp.path().join(".credentials.json"));

    fs::write(link_path.as_ref(), CREDS_V1).expect("write link");

    let barrier = Arc::new(Barrier::new(2));

    let b1 = barrier.clone();
    let ca1 = canonical.clone();
    let lp1 = link_path.clone();
    let t1 = std::thread::spawn(move || {
        b1.wait();
        sync_credentials_unlocked(&lp1, &ca1)
    });

    let b2 = barrier.clone();
    let ca2 = canonical.clone();
    let lp2 = link_path.clone();
    let t2 = std::thread::spawn(move || {
        b2.wait();
        sync_credentials_unlocked(&lp2, &ca2)
    });

    // one or both may error (same-PID tmp collision); end state is what matters
    let _ = t1.join().expect("thread 1 panicked");
    let _ = t2.join().expect("thread 2 panicked");

    // rename is atomic on POSIX — at least one thread wins; link_path must be a symlink
    assert!(
        link_path
            .symlink_metadata()
            .expect("link_path must exist")
            .file_type()
            .is_symlink(),
        "link_path must be a symlink after concurrent sync"
    );

    assert_eq!(
        fs::read(canonical.as_ref()).expect("read canonical"),
        CREDS_V1,
        "canonical must hold link content"
    );

    let tmp_name =
        link_path.with_file_name(format!(".credentials.json.tmp.{}", std::process::id()));
    assert!(
        !tmp_name.exists(),
        "PID-suffixed tmp must not persist after sync completes"
    );
}

// ── isolated runtime layout ──────────────────────────────────────────────────

/// Isolated mode omits operator memory/plugins/hooks but keeps account state.
#[test]
fn build_runtime_dir_isolated_omits_operator_extensions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("CLAUDE.md"), b"# operator memory").expect("write memory");
        fs::create_dir_all(claude_home.join("plugins")).expect("mkdir plugins");
        fs::create_dir_all(claude_home.join("hooks")).expect("mkdir hooks");
        fs::create_dir_all(claude_home.join("commands")).expect("mkdir commands");
        // Writable operator state that MUST NOT be shared: an isolated session's CC
        // (empty settings → default 30-day cleanupPeriodDays) would otherwise delete
        // the operator's transcripts through a shared `projects/` symlink.
        fs::write(claude_home.join("history.jsonl"), b"{}").expect("write history");
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::write(claude_home.join("stats-cache.json"), b"{}").expect("write stats");
        let runtime = tmp.path().join("runtime-isolated");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("iso");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Isolated,
        )
        .expect("build");

        // Skip-all: no operator house style AND no writable store is linked.
        for omitted in [
            "CLAUDE.md",
            "plugins",
            "hooks",
            "commands",
            "history.jsonl",
            "projects",
            "stats-cache.json",
        ] {
            assert!(
                !runtime.join(omitted).exists(),
                "isolated runtime must omit `{omitted}` (no shared writable state)"
            );
        }
        assert!(
            runtime.join("settings.json").exists(),
            "settings.json still written"
        );
    });
}

/// Shared mode keeps the same entries isolated mode strips — the control case.
#[test]
fn build_runtime_dir_shared_keeps_operator_extensions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("CLAUDE.md"), b"# operator memory").expect("write memory");
        fs::create_dir_all(claude_home.join("plugins")).expect("mkdir plugins");
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::write(claude_home.join("stats-cache.json"), b"{}").expect("write stats");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("shared");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        assert!(runtime.join("CLAUDE.md").exists(), "shared keeps memory");
        assert!(runtime.join("plugins").exists(), "shared keeps plugins");
        // The operator's own session shares the global writable store (project
        // history, aggregate) — the intentional contrast with isolated.
        assert!(runtime.join("projects").exists(), "shared keeps projects");
        assert!(
            runtime.join("stats-cache.json").exists(),
            "shared keeps stats-cache"
        );
    });
}

/// Isolated settings start from an empty base, so operator hooks never leak.
#[test]
fn build_runtime_dir_isolated_settings_drop_operator_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            claude_home.join("settings.json"),
            br#"{"hooks":{"PreToolUse":[]},"statusLine":{"type":"command"},"env":{"OP":"1"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime-isolated");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("iso");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Isolated,
        )
        .expect("build");

        let raw = fs::read_to_string(runtime.join("settings.json")).expect("read settings");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse settings");
        assert!(v.get("hooks").is_none(), "operator hooks dropped");
        assert!(v.get("statusLine").is_none(), "operator statusLine dropped");
        assert!(
            v["env"].get("OP").is_none(),
            "operator env entry dropped (empty base)"
        );
    });
}

/// A dangling top-level symlink (its `~/.claude/` source moved away) is removed
/// on the next build — the reported `runtime/CLAUDE.md.benchbak` leftover.
#[cfg(unix)]
#[test]
fn build_runtime_dir_prunes_dangling_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        // A link left from a prior build whose source no longer exists.
        let dangling = runtime.join("CLAUDE.md.benchbak");
        std::os::unix::fs::symlink(tmp.path().join("gone"), &dangling).expect("symlink");
        assert!(
            dangling.symlink_metadata().is_ok(),
            "link exists (dangling)"
        );
        assert!(!dangling.exists(), "target is gone");

        let profile = make_profile("heal");
        let canonical = tmp.path().join("creds.json");
        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        assert!(
            dangling.symlink_metadata().is_err(),
            "dangling symlink must be pruned on rebuild"
        );
    });
}

// ── isolation liveness + GC ──────────────────────────────────────────────────

/// THE LIVENESS GATE behind delete and disable. Every session now keys its
/// marker dir by session id, so the gate has to enumerate the profile dir
/// rather than probe two fixed names. A false negative here pulls an account
/// out from under a running session, so both flavors are pinned.
#[test]
fn has_live_session_sees_a_per_session_dir_of_either_flavor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        for (profile, sessions_name, sid) in [
            ("gate-shared", "sessions-31337-0", "31337-0"),
            ("gate-iso", "sessions-isolated-31337-1", "31337-1"),
        ] {
            let sessions = profiles.join(profile).join(sessions_name);
            fs::create_dir_all(&sessions).expect("mkdir sessions");
            let marker = open_pid_file(&sessions.join(sid)).expect("open marker");
            marker.lock().expect("lock marker");

            assert!(
                has_live_session(profile),
                "a live marker in {sessions_name} must gate rotation"
            );
            assert_eq!(live_session_count(profile), 1);

            drop(marker);
            // The probe is deliberately fail-alive (any try_lock I/O error reads
            // as "alive" — see `is_session_alive`), so only a PERSISTENTLY-alive
            // reading after the holder dropped is a regression.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while has_live_session(profile) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(!has_live_session(profile));
            assert_eq!(live_session_count(profile), 0);
        }
    });
}

/// The gate's fail-open must not cover the ENUMERATION step. `<profile>/` exists
/// for every configured profile (it holds `config.toml`, `credentials.json`,
/// `rotation.lock`), so its unreadability is not the idle case — a transient
/// EMFILE/EACCES reading as "no sessions" would unblock a rotation against a live
/// session. Only a genuinely absent dir is idle.
#[cfg(unix)]
#[test]
fn an_unreadable_profile_dir_reads_as_live_not_idle() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profile = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("unreadable");
        let sessions = profile.join("sessions-9001-0");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("9001-0"), b"").expect("dead marker");

        // Control: readable and genuinely idle.
        assert!(!has_live_session("unreadable"));
        // Control: never configured at all is still idle, not unknown.
        assert!(!has_live_session("never-started"));

        fs::set_permissions(&profile, fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read_dir(&profile).is_ok() {
            // Running with rights that ignore the mode (root); the probe cannot
            // be posed, so assert nothing rather than pass vacuously.
            fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("restore");
            return;
        }

        assert!(
            has_live_session("unreadable"),
            "an unreadable profile dir must read as live — a spurious false burns the chain"
        );
        assert_eq!(
            live_session_count("unreadable"),
            1,
            "the count must not contradict the gate within a tick"
        );

        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("restore");
    });
}

/// Same rule one level down: `live_sessions_at` distinguishes "absent" from
/// "could not tell", so each caller picks which way an unknown falls.
#[cfg(unix)]
#[test]
fn live_sessions_at_reports_unknown_separately_from_zero() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions-9002-0");
    fs::create_dir_all(&sessions).expect("mkdir sessions");

    assert_eq!(live_sessions_at(&tmp.path().join("absent")), Some(0));
    assert_eq!(live_sessions_at(&sessions), Some(0));

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read_dir(&sessions).is_ok() {
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
        return;
    }

    assert_eq!(
        live_sessions_at(&sessions),
        None,
        "an unreadable marker dir is unknown, never zero"
    );

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
}

/// The same rule at the DESTRUCTIVE level. `prune_stale_sessions` unlinks what
/// `is_session_alive` reads as dead, and its zero is what three callers turn into
/// `remove_dir_all` of a runtime tree — shared across the profile's sessions
/// under `LinkMode::Fake`. So an unopenable marker (EMFILE, ESTALE, EACCES) must
/// read LIVE: folding one into a false deletes a live session's only marker and
/// unblocks a rotation against the single-use token it still holds.
#[cfg(unix)]
#[test]
fn an_unopenable_marker_reads_as_live_and_is_never_unlinked() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions-9003-0");
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    let marker = sessions.join("9003-0");
    fs::write(&marker, b"").expect("marker");

    // Control: a readable, unlocked marker IS dead, and pruning removes it.
    assert!(!is_session_alive(&marker));
    assert_eq!(prune_stale_sessions(&sessions), Some(0));
    assert!(
        !marker.exists(),
        "a genuinely dead marker must be collected"
    );

    fs::write(&marker, b"").expect("re-create marker");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&marker)
        .is_ok()
    {
        // Running with rights that ignore the mode (root); the probe cannot be
        // posed, so assert nothing rather than pass vacuously.
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).expect("restore");
        return;
    }

    assert!(
        is_session_alive(&marker),
        "an unopenable marker is unknown, and unknown must read live"
    );
    assert_eq!(
        prune_stale_sessions(&sessions),
        Some(1),
        "an unopenable marker must not be counted as dead"
    );
    assert!(
        marker.exists(),
        "pruning unlinked a marker it could not prove dead"
    );

    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).expect("restore");
}

/// And the same rule for the marker DIR one level up. `prune_stale_sessions`'s
/// zero authorizes `remove_dir_all`, so an unreadable dir has to be an unknown —
/// `Some(0)` is reserved for a dir that is genuinely absent.
#[cfg(unix)]
#[test]
fn prune_reports_an_unreadable_marker_dir_as_unknown_not_zero() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions-9004-0");
    fs::create_dir_all(&sessions).expect("mkdir sessions");

    assert_eq!(
        prune_stale_sessions(&tmp.path().join("absent")),
        Some(0),
        "a genuinely absent dir is idle, not unknown"
    );
    assert_eq!(prune_stale_sessions(&sessions), Some(0));

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read_dir(&sessions).is_ok() {
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
        return;
    }

    assert_eq!(
        prune_stale_sessions(&sessions),
        None,
        "an unreadable marker dir must never authorize a teardown"
    );

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
}

/// GC hands `remove_dir_all` whatever it pairs, so it gates on the strict name
/// predicate. A profile child that merely starts with `runtime`/`sessions` is not
/// a runtime tree and must survive.
#[test]
fn gc_leaves_profile_children_that_only_look_like_runtime_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profile = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("bystander");
        fs::create_dir_all(&profile).expect("mkdir profile");

        let bystanders = [
            "runtime_state.json",
            "runtimes",
            "sessions.json",
            "runtime-isolatedish",
            "runtime-4242-x",
        ];
        for name in bystanders {
            let path = profile.join(name);
            if name.contains('.') {
                fs::write(&path, b"{}").expect("write bystander file");
            } else {
                fs::create_dir_all(&path).expect("mkdir bystander");
                fs::write(path.join("keep"), b"x").expect("seed bystander");
            }
        }

        gc_stale_runtimes();

        for name in bystanders {
            assert!(
                profile.join(name).exists(),
                "{name} is not a runtime tree and must not be collected"
            );
        }
    });
}

/// An isolated session must register as live so rotation never spends a token
/// it still holds — `has_live_session` unions both flavors.
#[test]
fn has_live_session_sees_isolated_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("iso")
            .join("sessions-isolated");
        fs::create_dir_all(&sessions).expect("mkdir isolated sessions");
        let pid = sessions.join("4242");
        let file = open_pid_file(&pid).expect("open pid");
        file.lock().expect("lock pid");
        assert!(has_live_session("iso"), "isolated live session counts");
        assert_eq!(live_session_count("iso"), 1);
        drop(file);
        // The probe is deliberately fail-alive (any try_lock I/O error reads
        // as "alive" — see `is_session_alive`), so transient errors under a
        // parallel suite run (fd pressure) can flip readings for a while. Poll
        // generously; only a PERSISTENT "alive" after the lock holder dropped
        // is a regression (flaked once under the full suite, 2026-07-12).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while has_live_session("iso") && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!has_live_session("iso"));
    });
}

/// GC removes a runtime tree left by a crashed session (no live PID), and never
/// touches one with a live session. All fixtures here are the LEGACY unsuffixed
/// layout a pre-per-session release left on disk: the `runtime<rest>` ↔
/// `sessions<rest>` pairing rule must reach it, which is the whole migration
/// path.
#[test]
fn gc_removes_stale_runtime_but_spares_live() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // Stale: a runtime tree with a dead (unlocked) pid file.
        let stale_runtime = profiles.join("stale").join("runtime");
        let stale_sessions = profiles.join("stale").join("sessions");
        fs::create_dir_all(&stale_runtime).expect("mkdir stale runtime");
        fs::create_dir_all(&stale_sessions).expect("mkdir stale sessions");
        fs::write(stale_runtime.join("settings.json"), b"{}").expect("seed runtime");
        fs::write(stale_sessions.join("99999"), b"").expect("dead pid");

        // Stale, isolated flavor: the same legacy shape one dir name over.
        let stale_iso_runtime = profiles.join("staleiso").join("runtime-isolated");
        let stale_iso_sessions = profiles.join("staleiso").join("sessions-isolated");
        fs::create_dir_all(&stale_iso_runtime).expect("mkdir stale iso runtime");
        fs::create_dir_all(&stale_iso_sessions).expect("mkdir stale iso sessions");
        fs::write(stale_iso_runtime.join(".claude.json"), b"{}").expect("seed iso runtime");
        fs::write(stale_iso_sessions.join("88888"), b"").expect("dead iso pid");

        // Live: an isolated runtime with a flock-held pid file.
        let live_runtime = profiles.join("live").join("runtime-isolated");
        let live_sessions = profiles.join("live").join("sessions-isolated");
        fs::create_dir_all(&live_runtime).expect("mkdir live runtime");
        fs::create_dir_all(&live_sessions).expect("mkdir live sessions");
        let held = open_pid_file(&live_sessions.join("1234")).expect("open live pid");
        held.lock().expect("lock live pid");

        gc_stale_runtimes();

        assert!(
            !stale_runtime.exists(),
            "stale runtime with no live session must be collected"
        );
        assert!(
            !stale_sessions.exists(),
            "stale sessions dir cleaned alongside"
        );
        assert!(
            !stale_iso_runtime.exists(),
            "a legacy isolated pair must be collected the same way"
        );
        assert!(!stale_iso_sessions.exists());
        assert!(
            live_runtime.exists(),
            "a live session's runtime must be spared"
        );
        drop(held);
    });
}

/// Per-session dirs are collected by the same pairing rule, both flavors, and a
/// held marker still spares its own pair.
#[test]
fn gc_collects_a_dead_per_session_pair_and_spares_a_held_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // Dead: both flavors, marker present but unlocked.
        let mut dead = Vec::new();
        for (profile, runtime_name, sid) in [
            ("psdead", "runtime-4242-0", "4242-0"),
            ("psdeadiso", "runtime-isolated-4242-1", "4242-1"),
        ] {
            let runtime = profiles.join(profile).join(runtime_name);
            let sessions = profiles.join(profile).join(
                runtime_name
                    .strip_prefix("runtime")
                    .map(|rest| format!("sessions{rest}"))
                    .expect("paired name"),
            );
            fs::create_dir_all(&runtime).expect("mkdir runtime");
            fs::create_dir_all(&sessions).expect("mkdir sessions");
            fs::write(runtime.join(".claude.json"), b"{}").expect("seed runtime");
            fs::write(sessions.join(sid), b"").expect("dead marker");
            dead.push((runtime, sessions));
        }

        // Held: a per-session pair whose marker is flock-held.
        let held_runtime = profiles.join("pslive").join("runtime-777-3");
        let held_sessions = profiles.join("pslive").join("sessions-777-3");
        fs::create_dir_all(&held_runtime).expect("mkdir held runtime");
        fs::create_dir_all(&held_sessions).expect("mkdir held sessions");
        fs::write(held_runtime.join(".claude.json"), b"{}").expect("seed held runtime");
        let marker = open_pid_file(&held_sessions.join("777-3")).expect("open held marker");
        marker.lock().expect("lock held marker");

        gc_stale_runtimes();

        for (runtime, sessions) in &dead {
            assert!(
                !runtime.exists(),
                "{} must be collected — its marker is unlocked",
                runtime.display()
            );
            assert!(!sessions.exists(), "{} must go with it", sessions.display());
        }
        assert!(
            held_runtime.join(".claude.json").is_file(),
            "a held marker must spare its own runtime tree and its contents"
        );
        assert!(held_sessions.join("777-3").is_file());
        drop(marker);
    });
}

/// `acquire` mints a marker dir before it builds the tree, so a crash in that
/// window strands one with no runtime sibling — a fresh empty dir every session
/// under per-session keying. GC must collect it, and must still leave one whose
/// marker is held.
#[test]
fn gc_collects_an_orphaned_sessions_dir_with_no_runtime_sibling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        let orphan = profiles.join("orphan").join("sessions-5150-0");
        fs::create_dir_all(&orphan).expect("mkdir orphan");
        fs::write(orphan.join("5150-0"), b"").expect("dead marker");

        let orphan_empty = profiles.join("orphan").join("sessions-5150-1");
        fs::create_dir_all(&orphan_empty).expect("mkdir empty orphan");

        let held = profiles.join("orphan").join("sessions-5150-2");
        fs::create_dir_all(&held).expect("mkdir held orphan");
        let marker = open_pid_file(&held.join("5150-2")).expect("open held marker");
        marker.lock().expect("lock held marker");

        gc_stale_runtimes();

        assert!(
            !orphan.exists(),
            "an orphaned marker dir with a dead marker must be collected"
        );
        assert!(
            !orphan_empty.exists(),
            "an orphaned marker dir with no marker at all must be collected"
        );
        assert!(
            held.join("5150-2").is_file(),
            "a still-held marker dir must be spared even with no runtime sibling"
        );
        drop(marker);
    });
}

/// Registry rows ride the same sweep as the dirs, keyed off the marker their own
/// fields name: a row whose marker is unlocked is dead, one whose marker is held
/// is not.
#[test]
fn gc_drops_a_registry_row_whose_marker_is_unlocked_and_keeps_a_held_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // Dead: marker file present but unlocked.
        let dead_markers = profiles.join("rowdead").join("sessions-6001-0");
        fs::create_dir_all(&dead_markers).expect("mkdir dead markers");
        fs::write(dead_markers.join("6001-0"), b"").expect("dead marker");

        // Held, isolated flavor — the marker path is derived from `isolated`.
        let held_markers = profiles.join("rowlive").join("sessions-isolated-6001-1");
        fs::create_dir_all(&held_markers).expect("mkdir held markers");
        let marker = open_pid_file(&held_markers.join("6001-1")).expect("open held marker");
        marker.lock().expect("lock held marker");

        let mut dead = crate::live_sessions::LiveSession {
            session_id: "6001-0".into(),
            start_profile: "rowdead".into(),
            pid: 6001,
            started_at: 1,
            cwd: None,
            isolated: false,
            follows_chain: false,
            intended_member: None,
            chain_cursor: None,
            current_member: None,
            last_swap_at: None,
        };
        crate::live_sessions::register(&dead).expect("register dead");
        dead.session_id = "6001-1".into();
        dead.start_profile = "rowlive".into();
        dead.isolated = true;
        crate::live_sessions::register(&dead).expect("register live");

        gc_stale_runtimes();

        let left: Vec<String> = crate::live_sessions::list()
            .into_iter()
            .map(|r| r.session_id)
            .collect();
        assert_eq!(
            left,
            vec!["6001-1".to_string()],
            "only the row whose marker is still flock-held may survive"
        );
        drop(marker);
    });
}

/// The wiring, end to end: a real `acquire` files a row carrying this session's
/// own identity, and its teardown takes the row with it.
#[test]
fn acquire_registers_a_row_and_teardown_removes_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("registered");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        let sid = sid_of(rt.config_dir());

        let rows = crate::live_sessions::list();
        assert_eq!(rows.len(), 1, "acquire must file exactly one row");
        let registered = &rows[0];
        assert_eq!(registered.session_id, sid);
        assert_eq!(registered.start_profile, "registered");
        assert_eq!(registered.pid, std::process::id());
        assert!(!registered.isolated);
        assert_eq!(registered.intended_member, None);
        assert_eq!(registered.current_member, None);
        assert_eq!(registered.chain_cursor, None);
        assert_eq!(registered.last_swap_at, None);

        drop(rt);

        assert!(
            crate::live_sessions::list().is_empty(),
            "teardown must take the session's row with it"
        );
    });
}

#[test]
fn scrub_profile_env_drops_managed_and_active_custom_keys() {
    // `clauth start <B>` from a session running profile A must not inherit A's
    // endpoint/auth/model overrides nor A's custom `[env]`. The target's
    // runtime settings.json re-supplies whichever it defines.
    let mut cmd = std::process::Command::new("claude");
    scrub_profile_env(&mut cmd, &["FOO".to_string()]);

    let envs = crate::testutil::env_overrides(&cmd);
    for key in MANAGED_ENV_KEYS {
        assert_eq!(
            envs.get(*key),
            Some(&None),
            "{key} must be stripped from the inherited env",
        );
    }
    assert_eq!(
        envs.get("FOO"),
        Some(&None),
        "active custom env key must be stripped",
    );
}

#[test]
fn cwd_is_real_home_matches_only_the_sandboxed_home() {
    let sandbox = HomeSandbox::new();
    assert!(cwd_is_real_home(sandbox.home()));

    let elsewhere = sandbox.home().join("repos").join("some-project");
    fs::create_dir_all(&elsewhere).expect("create project dir");
    assert!(!cwd_is_real_home(&elsewhere));
}

#[test]
fn guard_home_project_settings_appends_setting_sources_only_at_home() {
    let sandbox = HomeSandbox::new();

    let mut at_home = std::process::Command::new("claude");
    guard_home_project_settings(&mut at_home, sandbox.home());
    let args: Vec<_> = at_home
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        vec!["--setting-sources".to_string(), "user".to_string()],
        "cwd == $HOME must force the user-only settings tier"
    );

    let elsewhere = sandbox.home().join("repos").join("some-project");
    fs::create_dir_all(&elsewhere).expect("create project dir");
    let mut in_project = std::process::Command::new("claude");
    guard_home_project_settings(&mut in_project, &elsewhere);
    assert!(
        in_project.get_args().next().is_none(),
        "a normal project cwd must keep reading its own project settings"
    );
}

// ── per-session swap executor ────────────────────────────────────────────────

/// A chain member with a store on disk, told apart by its access token.
fn member(name: &str) -> Profile {
    let mut profile = make_profile(name);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(1_000),
            scopes: None,
            subscription_type: None,
        }),
    });
    profile
}

/// Persist `profile` and return the store a swap onto it repoints the link at.
fn member_store(profile: &Profile) -> PathBuf {
    crate::profile::save_profile(profile).expect("save member");
    crate::claude::install_source_path(profile.name.as_str()).expect("install source")
}

/// A live session with NO watchdog thread behind it, so every credential leg is
/// driven explicitly: a test asserting which leg moved which bytes can never be
/// won by a background tick landing first. `acquire` is used only where the
/// launch session's teardown is part of the assertion.
///
/// It stamps and HOLDS the launch member's markers exactly as `acquire` does. A
/// fixture that skipped them would let a swap back onto the launch member claim a
/// marker no production session could, so the returned locks are part of the
/// fixture, not litter.
fn lone_session(
    launch: &Profile,
    isolation: Isolation,
) -> (std::sync::Arc<SessionSwap>, SwappedMarkers) {
    let name = launch.name.as_str();
    let session = SessionId::mint();
    let store = crate::claude::install_source_path(name).expect("install source");
    let paths =
        SessionPaths::resolve(name, isolation, &session, LinkMode::Real).expect("session paths");
    crate::profile::mkdir_700(&paths.runtime).expect("mkdir runtime");
    create_symlink(&store, &paths.runtime.join(".credentials.json")).expect("link creds");
    let markers = stamp_swapped_markers(&paths)
        .expect("stamp launch markers")
        .expect("the launch member's markers must be free in a fresh sandbox");
    let row = crate::live_sessions::LiveSession::starting(
        &session,
        name,
        isolation == Isolation::Isolated,
        false,
    );
    crate::live_sessions::register(&row).expect("register row");
    let swap = std::sync::Arc::new(SessionSwap::new(
        session,
        isolation,
        LinkMode::Real,
        launch,
        store,
        &paths,
    ));
    (swap, markers)
}

/// The decision leg gates on `follows_chain` and nothing sets it true yet, so an
/// acquire-shaped registration must leave a session opted OUT — otherwise landing
/// the leg would move EVERY live session off the account it launched on.
#[test]
fn a_registered_session_is_opted_out_of_the_chain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("optin-a");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let row = crate::live_sessions::get(swap.session.as_str()).expect("the registered row");

        assert!(
            !row.follows_chain,
            "registration must not opt a session into the fallback chain"
        );
    });
}

/// `--with-fallback` is the only thing that sets `follows_chain`, so the flag has
/// to survive the whole way to the on-disk row: the decision leg reads that field
/// and nothing else decides whether a session is steerable.
#[cfg(not(target_os = "macos"))]
#[test]
fn an_opted_in_session_registers_as_following_the_chain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("optin-flag");

        let opted =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[], true).expect("acquire");
        let opted_row =
            crate::live_sessions::get(&sid_of(opted.config_dir())).expect("the opted-in row");
        drop(opted);

        let plain =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        let plain_row =
            crate::live_sessions::get(&sid_of(plain.config_dir())).expect("the opted-out row");
        drop(plain);

        assert!(
            opted_row.follows_chain,
            "--with-fallback must reach the registry row"
        );
        assert!(
            !plain_row.follows_chain,
            "a bare start must stay on its launch account"
        );
    });
}

/// The transport mode is known only INSIDE `acquire`'s state-lock hold, which is
/// also where the row is written. So the structural floor under the CLI refusal
/// lives there: a row claiming to follow the chain on a host whose executor
/// refuses every swap would collect daemon intents nothing can execute, each
/// announced exactly once into a log nobody is reading.
#[test]
fn a_fake_mode_host_never_registers_a_session_as_following_the_chain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("optin-fake");
        with_link_mode(LinkMode::Fake, || {
            let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], true)
                .expect("acquire under the shared fake-mode tree");
            let rows = crate::live_sessions::list();
            drop(rt);

            assert_eq!(rows.len(), 1, "exactly this session is registered");
            assert!(
                !rows[0].follows_chain,
                "a fake-mode row must never claim to follow the chain"
            );
        });
    });
}

/// The predicate behind that floor, spelled once and exercised on every arm —
/// `Isolated` and macOS are each unreachable through `acquire` from a Linux run,
/// and all three arms are refusals the executor also makes at its own chokepoint.
#[test]
fn a_chain_opt_in_survives_only_where_the_executor_can_swap() {
    assert!(
        chain_opt_in_survives(true, Isolation::Shared, LinkMode::Real, false),
        "a shared session on a real-symlink non-mac host is the supported case"
    );
    assert!(
        !chain_opt_in_survives(false, Isolation::Shared, LinkMode::Real, false),
        "nothing opts a session in but the flag"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Isolated, LinkMode::Real, false),
        "an isolated session follows no chain"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Shared, LinkMode::Fake, false),
        "a shared runtime tree cannot hold a per-session credential"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Shared, LinkMode::Real, true),
        "macOS resolves credentials keychain-first, so a file swap is inert"
    );
}

/// The platform arm answers with no disk at all, so `start::run` can refuse a
/// statically-known verdict without a probe that could time out on the state flock
/// or fail on IO. Pinned as a pure call because `cfg!(target_os = "macos")` makes
/// the arm unreachable from a Linux run any other way.
#[test]
fn the_swap_platform_verdict_needs_no_probe() {
    assert_eq!(
        unsupported_swap_platform(true),
        Some(SwapUnsupported::KeychainFirst),
        "macOS is refused off a compile-time constant"
    );
    assert_eq!(
        unsupported_swap_platform(false),
        None,
        "every other platform leaves the verdict to the transport probe"
    );
}

/// The pre-`acquire` transport half: `start::run` needs the verdict BEFORE a tree
/// is built or `claude` is spawned, and it can only get one by probing the profile
/// dir the way `acquire` does.
#[test]
fn the_swap_host_probe_names_each_unsupported_transport() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        assert_eq!(
            unsupported_swap_transport("probe-host").expect("probe"),
            None,
            "a real-symlink host supports the swap"
        );
        with_link_mode(LinkMode::Fake, || {
            assert_eq!(
                unsupported_swap_transport("probe-host").expect("probe"),
                Some(SwapUnsupported::SharedRuntimeTree),
                "a fake-symlink host shares one tree across the profile's sessions"
            );
        });
    });
}

/// The liveness predicate the decision leg gates every row on, anchored against the
/// REAL stamper rather than against a fixture that shares its path derivation. The
/// shared (non-isolated) layout is the production default and had no positive
/// anchor: a GC test asserting a row is DEAD passes for any wrong path, since a
/// marker that isn't there reads `NotFound` → dead.
#[test]
fn session_row_is_live_finds_the_marker_a_real_session_stamped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("rowlive-a");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str();

        assert!(
            session_row_is_live("rowlive-a", false, sid),
            "the probe must look where `stamp_swapped_markers` actually writes"
        );
        // The other direction, so the assert above cannot be won by a predicate that
        // reads everything as live: a session id nothing stamped is dead.
        assert!(
            !session_row_is_live("rowlive-a", false, "9999-0"),
            "an unstamped session id must read dead"
        );
    });
}

/// Every member in one config, each carrying a refresh token, so only the
/// live-session gate can keep it out of `rotation_candidates`.
#[cfg(not(target_os = "macos"))]
fn config_of(members: &[&Profile]) -> crate::profile::AppConfig {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: members.iter().map(|p| (*p).clone()).collect(),
    };
    for profile in members {
        config.state.profiles.push(profile.name.clone());
    }
    config
}

/// A Claude Code re-login as it lands on disk: the runtime link replaced by a
/// regular file, mtime `when` so the recency compare is unambiguous.
#[cfg(not(target_os = "macos"))]
fn cc_relogin(runtime: &Path, bytes: &[u8], when: SystemTime) -> PathBuf {
    let link = runtime.join(".credentials.json");
    let _ = fs::remove_file(&link);
    fs::write(&link, bytes).expect("write relogin");
    set_mtime(&link, when);
    link
}

/// THE §12 TEST. Claude Code stats the mtime of the symlink's TARGET at the head
/// of every request and re-reads only when that value CHANGED, so an
/// mtime-preserving repoint is a SILENT no-op: the session keeps authenticating
/// as the old member and nothing anywhere reports a problem.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_moves_the_mtime_of_the_store_it_repoints_to() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("mtime-a");
        let intended = member("mtime-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // ONE shared mtime — the pathological case the live probe found, where
        // repointing the link changes nothing Claude Code can observe.
        let shared = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&launch_store, shared);
        set_mtime(&intended_store, shared);

        let before_swap = SystemTime::now();
        assert_eq!(swap.swap_to("mtime-b").expect("swap"), SwapOutcome::Swapped);

        let after = fs::metadata(&intended_store)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert!(
            after > shared,
            "the store CC stats through the link kept its mtime, \
             so this swap is a silent no-op"
        );
        // WHICH mechanism moved it, not just that something did: the swap stamps
        // the CLOCK. A value derived from the old store's mtime clears the assert
        // above too, while carrying that store's skew onto this one.
        assert!(
            after >= before_swap,
            "the new store's mtime must come from the clock, not from the old store"
        );
    });
}

/// B2. The touch above makes the intended member's store strictly newer, and
/// `profile::recover_pending_credentials` adopts a `credentials.json.pending`
/// sidecar only while it is at least as new as the store — so a sidecar left by a
/// rotation that died mid-save would be silently discarded, losing a refresh pair
/// that may be the only live one. `load_profile` adopting it first is what makes
/// the touch safe, and the plan the touch requires is minted by that load.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_adopts_a_crash_staged_sidecar_before_moving_the_store_mtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("stage-a");
        let intended = member("stage-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // A rotation that staged its new pair and died before the commit.
        let staged = ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-rotated".into(),
                refresh_token: Some("rt-rotated".into()),
                expires_at: Some(9_000),
                scopes: None,
                subscription_type: None,
            }),
        };
        crate::profile::stage_rotated_credentials("stage-b", &staged).expect("stage");
        let sidecar = crate::profile::profile_dir("stage-b")
            .expect("profile dir")
            .join("credentials.json.pending");
        assert!(
            sidecar.is_file(),
            "fixture: the sidecar must exist pre-swap"
        );

        assert_eq!(swap.swap_to("stage-b").expect("swap"), SwapOutcome::Swapped);

        let store: ClaudeCredentials =
            serde_json::from_slice(&fs::read(&intended_store).expect("read store"))
                .expect("parse store");
        assert_eq!(
            store.claude_ai_oauth.and_then(|o| o.refresh_token),
            Some("rt-rotated".to_string()),
            "the staged rotation must be adopted before the store's mtime moves, \
             or the touch discards it and the refresh pair is gone"
        );
        assert!(
            !sidecar.exists(),
            "an adopted sidecar must be removed, so nothing can re-adopt it later"
        );
    });
}

/// The platform/transport gate is PURE so both refusals are exercised from a
/// Linux run. A swap that silently leaves the session on its launch account is
/// the one outcome §12 exists to prevent, so refusing loudly is the requirement.
#[test]
fn swap_support_refuses_a_shared_tree_and_a_keychain_first_host() {
    assert_eq!(
        swap_support(LinkMode::Fake, false),
        Err(SwapUnsupported::SharedRuntimeTree)
    );
    assert_eq!(
        swap_support(LinkMode::Fake, true),
        Err(SwapUnsupported::SharedRuntimeTree)
    );
    assert_eq!(
        swap_support(LinkMode::Real, true),
        Err(SwapUnsupported::KeychainFirst)
    );
    assert_eq!(swap_support(LinkMode::Real, false), Ok(()));
}

/// The rotation refusal is macOS-ONLY and pure, so both arms run from a Linux
/// box. It exists because clauth cannot write the Keychain item a `clauth start`
/// session's Claude Code reads (that item is namespaced per `CLAUDE_CONFIG_DIR`;
/// `keychain::SERVICE` is the unsuffixed one), so a rotation there signs the
/// session out rather than propagating to it.
#[test]
fn rotation_is_blocked_by_a_live_session_only_on_macos() {
    // macOS: a live `clauth start` session is the whole refusal.
    assert!(rotation_blocked_by_live_session(true, true));
    assert!(!rotation_blocked_by_live_session(false, true));
    // Everywhere else the session shares the credential FILE clauth rotates,
    // so it follows the new pair on its next request.
    assert!(!rotation_blocked_by_live_session(true, false));
    assert!(!rotation_blocked_by_live_session(false, false));
}

/// The profile-comparison half of the precondition, as a pure function the
/// daemon's per-session walk shares. It exists so the two cannot drift: a
/// candidate the executor refuses on CONFIG grounds has to be walked PAST, or the
/// intent never changes and the session never reaches the next viable member.
/// One case per arm, and the api-key arm both directions — it compares STATES, so
/// a launch that has a key needs a candidate that has one too.
#[test]
fn swap_eligible_refuses_exactly_the_config_grounds_the_precondition_does() {
    let mut launch = make_profile("elig-launch");
    launch.env.insert("SHARED".into(), "1".into());
    launch.models.default = Some("sonnet".into());
    let transport = LaunchTransport::of(&launch);

    let mut twin = make_profile("elig-twin");
    twin.env = launch.env.clone();
    twin.models = launch.models.clone();
    assert_eq!(swap_eligible(&twin, &transport), Ok(()));

    let mut endpoint = twin.clone();
    endpoint.base_url = Some("https://api.example/anthropic".into());
    assert_eq!(
        swap_eligible(&endpoint, &transport),
        Err(SwapRefused::NotOauth)
    );

    let mut disabled = twin.clone();
    disabled.disabled = true;
    assert_eq!(
        swap_eligible(&disabled, &transport),
        Err(SwapRefused::Disabled)
    );

    let mut env = twin.clone();
    env.env.insert("EXTRA".into(), "2".into());
    assert_eq!(
        swap_eligible(&env, &transport),
        Err(SwapRefused::EnvDiffers)
    );

    let mut models = twin.clone();
    models.models.default = Some("opus".into());
    assert_eq!(
        swap_eligible(&models, &transport),
        Err(SwapRefused::ModelsDiffers)
    );

    let mut keyed = twin.clone();
    keyed.api_key = Some("k".into());
    assert_eq!(
        swap_eligible(&keyed, &transport),
        Err(SwapRefused::ApiKeyDiffers)
    );

    // ORDER is observable, not incidental: `announce_refusal` dedupes per
    // `(member, reason)`, so which cause an operator is told for a member hitting
    // two arms is decided here. Nothing couples `base_url` to `disabled`, so a
    // disabled third-party member hits both — the endpoint is the cause to report,
    // since it disqualifies the member even once re-enabled.
    let mut endpoint_and_disabled = endpoint.clone();
    endpoint_and_disabled.disabled = true;
    assert_eq!(
        swap_eligible(&endpoint_and_disabled, &transport),
        Err(SwapRefused::NotOauth),
        "the endpoint must be reported ahead of the disabled bit"
    );

    // A session launched on an api-key member: the same-state candidate clears
    // and the keyless one is the one refused, so the compare cannot degrade into
    // "the candidate has no key".
    let mut keyed_launch = launch.clone();
    keyed_launch.api_key = Some("k".into());
    let keyed_transport = LaunchTransport::of(&keyed_launch);
    assert_eq!(swap_eligible(&keyed, &keyed_transport), Ok(()));
    assert_eq!(
        swap_eligible(&twin, &keyed_transport),
        Err(SwapRefused::ApiKeyDiffers)
    );
}

/// `settings.json` env reaches Claude Code's `process.env` only at STARTUP,
/// while `ANTHROPIC_AUTH_TOKEN` is read live per client construction, so a
/// member carrying different env or model routing is a genuinely different
/// transport rather than the same account elsewhere.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_precondition_refuses_a_member_whose_transport_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pre-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let twin = member("pre-twin");
        member_store(&twin);

        let mut env = member("pre-env");
        env.env.insert("SOME_KEY".into(), "1".into());
        member_store(&env);

        let mut models = member("pre-models");
        models.models.default = Some("opus".into());
        member_store(&models);

        // base_url + api key + NO stored pair: `load_profile` normalizes a
        // base_url away only when a pair is stored and no usable key is.
        let mut endpoint = make_profile("pre-endpoint");
        endpoint.base_url = Some("https://api.example/anthropic".into());
        endpoint.api_key = Some("k".into());
        crate::profile::save_profile(&endpoint).expect("save endpoint");

        let mut disabled = member("pre-disabled");
        disabled.disabled = true;
        member_store(&disabled);

        let mut keyed = member("pre-keyed");
        keyed.api_key = Some("k".into());
        member_store(&keyed);

        // The cleared case yields the plan the touch step needs, keyed to the
        // member it loaded.
        let cleared = |name: &str| swap.precondition(name).map(|plan| plan.member);
        assert_eq!(cleared("pre-twin"), Ok("pre-twin".to_string()));
        assert_eq!(cleared("pre-env"), Err(SwapRefused::EnvDiffers));
        assert_eq!(cleared("pre-models"), Err(SwapRefused::ModelsDiffers));
        assert_eq!(cleared("pre-endpoint"), Err(SwapRefused::NotOauth));
        assert_eq!(cleared("pre-disabled"), Err(SwapRefused::Disabled));
        assert_eq!(cleared("pre-keyed"), Err(SwapRefused::ApiKeyDiffers));
        assert_eq!(cleared("pre-absent"), Err(SwapRefused::NoCredentialStore));
    });
}

/// A clauth predating the per-session layout probes exactly `<profile>/sessions`,
/// so without a marker there its `has_live_session` reads the swapped-onto member
/// as IDLE and its rotation leg spends the single-use refresh token the live
/// Claude Code child is authenticating with. Right after an upgrade that old
/// binary is the running daemon.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_holds_both_of_the_intended_members_liveness_markers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("marker-a");
        let intended = member("marker-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        assert_eq!(
            swap.swap_to("marker-b").expect("swap"),
            SwapOutcome::Swapped
        );

        let profile_dir = crate::profile::profile_dir("marker-b").expect("profile dir");
        for marker in [
            profile_dir.join(format!("sessions-{sid}")).join(&sid),
            profile_dir.join("sessions").join(&sid),
        ] {
            assert!(marker.is_file(), "no marker at {}", marker.display());
            assert!(
                is_session_alive(&marker),
                "{} must be flock-held for the session's life",
                marker.display()
            );
        }
        assert!(
            has_live_session("marker-b"),
            "the rotation gate must see the swapped-onto member as live"
        );
    });
}

/// A member whose marker this session cannot hold is a member the rotation gate
/// cannot see it on, so the swap refuses INSIDE the hold rather than repointing
/// the link at a chain nothing is protecting.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_refuses_a_member_whose_marker_another_process_holds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("held-a");
        let intended = member("held-b");
        let launch_store = member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        // A live foreign process already owns the per-session marker path.
        let markers = crate::profile::profile_dir("held-b")
            .expect("profile dir")
            .join(format!("sessions-{sid}"));
        fs::create_dir_all(&markers).expect("mkdir markers");
        let held = open_pid_file(&markers.join(&sid)).expect("open marker");
        held.lock().expect("lock marker");

        let outcome = swap.swap_to("held-b").expect("swap");

        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            launch_store,
            "a refused swap must leave the link on the member it was protecting"
        );
        assert_eq!(
            outcome,
            SwapOutcome::Refused(SwapRefused::MarkerNotLockable)
        );
        drop(held);
    });
}

/// §11 step 8: the previous member's marker is NEVER dropped, because the live
/// Claude Code child still holds its refresh token in memory and nothing can
/// observe when it stops. The marker is liveness bookkeeping the destructive
/// guards read — it is NOT a rotation gate, so both members stay rotatable
/// throughout. A swapped session follows whichever pair clauth writes.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_keeps_both_members_marked_live_and_still_rotatable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let launch = member("rot-a");
        let intended = member("rot-b");
        member_store(&launch);
        member_store(&intended);

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false).expect("acquire");
        let sid = live_sid(&rt);
        assert_eq!(
            rt.swap().swap_to("rot-b").expect("swap"),
            SwapOutcome::Swapped
        );

        let launch_dir = crate::profile::profile_dir("rot-a").expect("profile dir");
        for marker in [
            launch_dir.join(format!("sessions-{sid}")).join(&sid),
            launch_dir.join("sessions").join(&sid),
        ] {
            assert!(
                is_session_alive(&marker),
                "{} must survive the swap — the live child still holds that chain",
                marker.display()
            );
        }

        let config = config_of(&[&launch, &intended]);
        let want = vec![
            ("rot-a".to_string(), "rt-rot-a".to_string()),
            ("rot-b".to_string(), "rt-rot-b".to_string()),
        ];
        assert_eq!(
            crate::oauth::rotation_candidates(&config, false),
            want,
            "a live marker is not a rotation gate — both members stay candidates"
        );
        assert_eq!(
            crate::oauth::rotation_candidates(&config, true),
            want,
            "force changes nothing here; liveness never excluded either member"
        );
        drop(rt);
    });
}

/// The repoint itself: `.credentials.json` resolves to the intended member's
/// store, through the tmp+rename swap rather than a remove+create.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_repoints_the_runtime_link_at_the_intended_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("link-a");
        let intended = member("link-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let link = swap.runtime.join(".credentials.json");

        assert_eq!(swap.swap_to("link-b").expect("swap"), SwapOutcome::Swapped);

        assert_eq!(
            fs::read_link(&link).expect("read link"),
            intended_store,
            "the credential link must resolve to the intended member's store"
        );
        assert_eq!(
            fs::read(&link).expect("read through link"),
            fs::read(&intended_store).expect("read store"),
        );
    });
}

/// §11 #1. A Claude Code re-login sitting in the runtime file belongs to the
/// member the link STILL resolves to; without the drain those bytes land in the
/// new member's store on the next tick and its refresh token is gone.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_drains_a_pending_relogin_into_the_launch_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("drain-a");
        let intended = member("drain-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let intended_before = fs::read(&intended_store).expect("read intended store");
        set_mtime(&launch_store, SystemTime::now() - Duration::from_secs(60));
        cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

        assert_eq!(swap.swap_to("drain-b").expect("swap"), SwapOutcome::Swapped);

        assert_eq!(
            fs::read(&launch_store).expect("read launch store"),
            CREDS_V2,
            "the re-login must be captured into the member the link still resolved to"
        );
        assert_eq!(
            fs::read(&intended_store).expect("read intended store"),
            intended_before,
            "the intended member's own chain must be untouched by the drain"
        );
    });
}

/// B5. The watchdog thread and `Drop`'s final tick both used to read a MOVED
/// CLONE of `canonical`, so a swap that only mutated a field would have the next
/// tick relink the session back to the OLD member AND write the new member's
/// tokens into the old member's store.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_tick_after_a_swap_drains_into_the_intended_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let launch = member("tick-a");
        let intended = member("tick-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        assert_eq!(swap.swap_to("tick-b").expect("swap"), SwapOutcome::Swapped);

        let launch_before = fs::read(&launch_store).expect("read launch store");
        set_mtime(&intended_store, SystemTime::now() - Duration::from_secs(60));
        let link = cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

        tick(&claude_home, &swap).expect("tick");

        assert_eq!(
            fs::read(&intended_store).expect("read intended store"),
            CREDS_V2,
            "the tick must drain into the member the swap published, not the launch one"
        );
        assert_eq!(
            fs::read(&launch_store).expect("read launch store"),
            launch_before,
            "the launch member's store must never receive the new member's bytes"
        );
        assert_eq!(
            fs::read_link(&link).expect("read link"),
            intended_store,
            "the tick must re-establish the link to the intended member"
        );
    });
}

/// B6. `<intended>/sessions-<sid>/` has no `runtime-<sid>` sibling, so it lands
/// in `gc_stale_runtimes`'s orphaned-marker-dir arm. It is spared only because
/// the flock the swap holds reads live — one edit away from deleting a live
/// session's rotation protection.
#[cfg(not(target_os = "macos"))]
#[test]
fn gc_spares_a_swapped_members_marker_dir_while_the_session_lives() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("gc-a");
        let intended = member("gc-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        assert_eq!(swap.swap_to("gc-b").expect("swap"), SwapOutcome::Swapped);

        let profile_dir = crate::profile::profile_dir("gc-b").expect("profile dir");
        let own = profile_dir.join(format!("sessions-{sid}"));
        let compat = profile_dir.join("sessions");

        gc_stale_runtimes();
        assert!(
            is_session_alive(&own.join(&sid)),
            "GC collected a live session's per-session marker on the swapped-onto member"
        );
        assert!(
            is_session_alive(&compat.join(&sid)),
            "GC collected a live session's upgrade-compat marker on the swapped-onto member"
        );

        drop(swap);
        gc_stale_runtimes();
        assert!(
            !own.exists(),
            "the marker dir must be collected once its session is gone"
        );
        assert!(!compat.exists(), "so must the compat dir");
    });
}

/// Teardown owns every marker the session stamped — both layouts, on the launch
/// member and on each member it swapped onto — or a dead session keeps blocking
/// rotation on accounts nothing is using.
#[cfg(not(target_os = "macos"))]
#[test]
fn teardown_removes_every_marker_a_swap_stamped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let launch = member("down-a");
        let intended = member("down-b");
        member_store(&launch);
        member_store(&intended);

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false).expect("acquire");
        let sid = live_sid(&rt);
        assert_eq!(
            rt.swap().swap_to("down-b").expect("swap"),
            SwapOutcome::Swapped
        );

        let mut markers = Vec::new();
        for name in ["down-a", "down-b"] {
            let dir = crate::profile::profile_dir(name).expect("profile dir");
            markers.push(dir.join(format!("sessions-{sid}")).join(&sid));
            markers.push(dir.join("sessions").join(&sid));
        }
        for marker in &markers {
            assert!(is_session_alive(marker), "{} not held", marker.display());
        }

        drop(rt);

        for marker in &markers {
            assert!(
                !marker.exists(),
                "teardown left {} behind, blocking rotation on a dead session",
                marker.display()
            );
        }
        assert!(
            !has_live_session("down-b"),
            "the swapped-onto member must be rotatable again once the session exits"
        );
    });
}

/// Phase 0b's discipline, now on the swap path: `stamp_legacy_marker` yields
/// `None` when `try_lock` loses to a live process that minted the same sid, and
/// unlinking there deletes a FOREIGN session's liveness signal — the same
/// rotation burn the compat marker exists to prevent.
#[cfg(not(target_os = "macos"))]
#[test]
fn teardown_leaves_a_swapped_compat_marker_it_never_owned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let launch = member("foreign-a");
        let intended = member("foreign-b");
        member_store(&launch);
        member_store(&intended);

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false).expect("acquire");
        let sid = live_sid(&rt);

        // A live foreign holder already owns the compat path on the member we are
        // about to swap onto.
        let compat = crate::profile::profile_dir("foreign-b")
            .expect("profile dir")
            .join("sessions");
        fs::create_dir_all(&compat).expect("mkdir compat");
        let foreign = compat.join(&sid);
        let held = open_pid_file(&foreign).expect("open foreign marker");
        held.lock().expect("lock foreign marker");

        assert_eq!(
            rt.swap().swap_to("foreign-b").expect("swap"),
            SwapOutcome::Swapped
        );

        drop(rt);

        assert!(
            foreign.is_file(),
            "teardown unlinked a compat marker owned by another live process"
        );
        assert!(
            is_session_alive(&foreign),
            "the foreign holder's flock must be untouched"
        );
        drop(held);
    });
}

/// A swap onto the member the link already resolves to must touch nothing: no
/// marker on a second path, no mtime move that would make Claude Code re-read
/// for no reason, no registry write.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_onto_the_member_already_current_changes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("noop-a");
        let launch_store = member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&launch_store, before);

        // The side effects are asserted BEFORE the outcome: an outcome assert
        // first would panic on any mutation that lets the swap run, so the
        // touches-nothing claims would never be reached.
        let outcome = swap.swap_to("noop-a").expect("swap");

        assert_eq!(
            fs::metadata(&launch_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before,
            "a no-op swap must not move the store's mtime"
        );
        // Not evidence on its own — a same-member swap would resolve to the
        // launch marker and claim nothing anyway. It pins the narrower thing: a
        // `claim_markers` that stamped unconditionally.
        assert!(
            swap.cell().held.is_empty(),
            "a no-op swap must not claim a marker"
        );
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(
            row.current_member, None,
            "a no-op swap must not write the row"
        );
        assert_eq!(row.last_swap_at, None);
        assert_eq!(outcome, SwapOutcome::Refused(SwapRefused::AlreadyCurrent));
    });
}

/// §11 #11. The daemon writes `intended_member` while the session executes; a row
/// loaded before the swap and stored after would silently revert it, and the
/// session would keep re-swapping onto a member the daemon has moved past.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_preserves_a_daemon_written_intended_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("row-a");
        let intended = member("row-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        crate::live_sessions::update_as_daemon(&sid, |d| {
            d.set_intended_member("row-b");
            d.set_chain_cursor(2);
        })
        .expect("daemon write");

        assert_eq!(swap.swap_to("row-b").expect("swap"), SwapOutcome::Swapped);

        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(
            row.intended_member.as_deref(),
            Some("row-b"),
            "the session's own write must not revert a daemon-owned field"
        );
        assert_eq!(row.chain_cursor, Some(2));
        assert_eq!(row.current_member.as_deref(), Some("row-b"));
        assert!(row.last_swap_at.is_some());
    });
}

/// §11 #12's residue, bounded where it is cheap: `Drop` joins the watchdog, so a
/// swap STARTED after teardown began would hold session exit for the state-lock
/// timeout plus an unbounded rotation-flock wait.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_does_not_start_once_teardown_has_begun() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("bye-a");
        let intended = member("bye-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&intended_store, before);
        swap.begin_shutdown();

        assert_eq!(
            swap.precondition("bye-b").map(|plan| plan.member),
            Err(SwapRefused::ShuttingDown)
        );
        assert_eq!(
            swap.swap_to("bye-b").expect("swap"),
            SwapOutcome::Refused(SwapRefused::ShuttingDown)
        );
        assert_eq!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before,
            "a refused swap must touch nothing"
        );
    });
}

/// THE RECOVERY HALF OF THE CHAIN. `flock` locks the open file description, so a
/// second `open` + `try_lock` from THIS process is denied by our OWN lock — and
/// this session never releases a marker (step 8). So a swap back onto a member it
/// has already run on has to recognize the marker as already ours; reading it as a
/// foreign holder would refuse every recovery hop for the session's whole life,
/// after exactly one log line.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_back_onto_a_member_the_session_already_ran_on_succeeds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("back-a");
        let intended = member("back-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let link = swap.runtime.join(".credentials.json");

        assert_eq!(swap.swap_to("back-b").expect("out"), SwapOutcome::Swapped);
        let away = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&launch_store, away);

        assert_eq!(
            swap.swap_to("back-a").expect("back"),
            SwapOutcome::Swapped,
            "a member this session already holds a marker on is not a foreign holder"
        );

        assert_eq!(
            fs::read_link(&link).expect("read link"),
            launch_store,
            "the link must resolve back to the recovered member's store"
        );
        assert!(
            fs::metadata(&launch_store)
                .expect("meta")
                .modified()
                .expect("mtime")
                > away,
            "the recovered store's mtime must move, or Claude Code never re-reads it"
        );
        for name in ["back-a", "back-b"] {
            assert!(
                has_live_session(name),
                "{name} must stay rotation-blocked: the child still holds its chain"
            );
        }
        assert!(
            fs::metadata(&intended_store).is_ok(),
            "fixture: the member swapped away from keeps its own store"
        );
    });
}

/// A repoint that fails leaves the session authenticating as the member its link
/// still resolves to, so the cell must not have moved: a cell pointing at the
/// intended member while the link resolves to the launch one is §12's silent
/// no-op reached through an error path, permanent (`poll` filters on
/// `member()` equality) and reported by one log line.
#[cfg(all(unix, not(target_os = "macos")))]
#[cfg(not(target_os = "macos"))]
#[test]
fn a_failed_repoint_leaves_the_session_on_the_member_its_link_resolves_to() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let launch = member("fail-a");
        let intended = member("fail-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // The repoint stages a temp symlink INSIDE the runtime dir, so a
        // write-denied dir is what makes `relink_to_canonical` fail.
        fs::set_permissions(&swap.runtime, fs::Permissions::from_mode(0o500)).expect("chmod");
        if create_symlink(&launch_store, &swap.runtime.join(".probe")).is_ok() {
            // Running with rights that ignore the mode (root): the probe cannot be
            // posed, so assert nothing rather than pass vacuously.
            let _ = fs::remove_file(swap.runtime.join(".probe"));
            fs::set_permissions(&swap.runtime, fs::Permissions::from_mode(0o700)).expect("restore");
            return;
        }

        assert!(
            swap.swap_to("fail-b").is_err(),
            "fixture: the repoint must actually fail for this test to pose anything"
        );
        fs::set_permissions(&swap.runtime, fs::Permissions::from_mode(0o700)).expect("restore");

        assert_eq!(
            swap.canonical(),
            launch_store,
            "the cell moved onto a member the link never reached"
        );

        // The consequence, if it had moved: an interactive `/login` in the session
        // belongs to the LAUNCH account, and the tick would write it over the
        // intended member's store, destroying a chain nothing ever used.
        let intended_before = fs::read(&intended_store).expect("read intended");
        set_mtime(&launch_store, SystemTime::now() - Duration::from_secs(60));
        cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());
        tick(&claude_home, &swap).expect("tick");

        assert_eq!(
            fs::read(&launch_store).expect("read launch"),
            CREDS_V2,
            "the re-login belongs to the member the link resolved to"
        );
        assert_eq!(
            fs::read(&intended_store).expect("read intended"),
            intended_before,
            "a member the session never authenticated as must keep its own chain"
        );
    });
}

/// What Claude Code compares is EQUALITY against the mtime it memoized for the
/// previous target (`if(e!==Oeu)`), so the swap only has to make the new store's
/// value DIFFER — and must not reach that by importing the old store's clock
/// skew. A store left ahead of the clock makes `recover_pending_credentials`
/// discard every later crash-staged sidecar and `resolve_credential_winner`
/// discard every later re-login, on a member whose mtime was healthy until the
/// swap touched it.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_moves_the_mtime_without_importing_the_old_stores_skew() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("ahead-a");
        let intended = member("ahead-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // The store the link came from is stamped an hour ahead of the clock — a
        // restored backup, a skewed network mount.
        let ahead = SystemTime::now() + Duration::from_secs(3_600);
        set_mtime(&launch_store, ahead);

        assert_eq!(swap.swap_to("ahead-b").expect("out"), SwapOutcome::Swapped);

        let after = fs::metadata(&intended_store)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert_ne!(
            after, ahead,
            "the new target's mtime must differ from the one CC memoized"
        );
        assert!(
            after <= SystemTime::now(),
            "the swap stamped the new member's store ahead of the clock, which \
             discards its later sidecars and re-logins for as long as it stands"
        );
    });
}

/// `--isolated` and fallback-following are mutually exclusive (settled). The
/// executor is the single chokepoint every phase goes through, so the refusal
/// lives here rather than being re-remembered by the decision leg and the flag.
#[cfg(not(target_os = "macos"))]
#[test]
fn an_isolated_session_never_swaps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("iso-a");
        let intended = member("iso-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Isolated);

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&intended_store, before);

        let outcome = swap.swap_to("iso-b").expect("out");
        assert_eq!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before,
            "a refused swap must touch nothing"
        );
        assert_eq!(outcome, SwapOutcome::Refused(SwapRefused::IsolatedSession));
    });
}

// ── the watchdog's swap leg (`poll`) ─────────────────────────────────────────

/// The shipped inertness: nothing writes `intended_member` until the decision leg
/// lands, so `poll` must be a no-op on every tick of every session today.
#[test]
fn poll_does_nothing_until_the_daemon_names_a_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("poll-a");
        let intended = member("poll-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&intended_store, before);

        swap.poll();

        assert_eq!(swap.member(), "poll-a", "no intent, no move");
        assert_eq!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before
        );
        assert_eq!(
            crate::live_sessions::get(&sid).expect("row").current_member,
            None
        );

        // An intent naming the member the link already resolves to is the steady
        // state, not a refusal — it must stay silent too.
        crate::live_sessions::update_as_daemon(&sid, |d| d.set_intended_member("poll-a"))
            .expect("daemon write");
        swap.poll();
        assert_eq!(swap.member(), "poll-a");
        assert_eq!(
            crate::live_sessions::get(&sid).expect("row").current_member,
            None,
            "an intent equal to the current member must not write the row"
        );
        assert!(
            swap.cell().last_refusal.is_none(),
            "the steady state is not a refusal — routing it through one would log \
             a line per tick for as long as the intent stands"
        );
    });
}

/// The production trigger: the session's own tick reads its own row and executes.
#[cfg(not(target_os = "macos"))]
#[test]
fn poll_executes_the_member_the_daemon_named() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("polled-a");
        let intended = member("polled-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        crate::live_sessions::update_as_daemon(&sid, |d| d.set_intended_member("polled-b"))
            .expect("daemon write");

        swap.poll();

        assert_eq!(swap.member(), "polled-b");
        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            intended_store
        );
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(row.current_member.as_deref(), Some("polled-b"));
        assert_eq!(row.intended_member.as_deref(), Some("polled-b"));
    });
}

/// A standing intent the executor refuses re-fires every tick, so announcing
/// unconditionally writes one line per second for as long as it stands — but a
/// refusal nothing ever says leaves the session on its launch account invisibly.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_standing_refusal_is_announced_once_per_reason() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("say-a");
        let intended = member("say-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        assert!(
            swap.should_announce("say-b", &SwapRefused::NotOauth),
            "the first refusal must be said"
        );
        assert!(
            !swap.should_announce("say-b", &SwapRefused::NotOauth),
            "the same refusal, standing, must not repeat every tick"
        );
        assert!(
            swap.should_announce("say-b", &SwapRefused::Disabled),
            "a changed reason is news"
        );
        assert!(
            swap.should_announce("say-c", &SwapRefused::Disabled),
            "a changed member is news"
        );

        // A landed swap resets it: the next refusal on that member is new
        // information, not a repeat.
        assert!(!swap.should_announce("say-c", &SwapRefused::Disabled));
        assert_eq!(swap.swap_to("say-b").expect("out"), SwapOutcome::Swapped);
        assert!(
            swap.should_announce("say-c", &SwapRefused::Disabled),
            "a swap clears the announced state"
        );
    });
}

// ── bare `claude` session markers ────────────────────────────────────────────

/// The whole safety argument for counting bare sessions: their markers live
/// OUTSIDE `profiles/`, so `has_live_session` — which gates delete, disable, and
/// every macOS rotation leg — reads exactly the `clauth start` sessions it read
/// before. Both directions, because a marker namespace that suppressed a real
/// session's marker would be the same defect pointing the other way.
#[test]
fn a_bare_session_marker_is_invisible_to_has_live_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _bare = register_bare_session().expect("register a bare session");
        assert_eq!(
            live_bare_sessions(),
            Some(1),
            "fixture control: the marker must actually be held"
        );

        assert!(
            !has_live_session("work"),
            "a bare `claude` must not gate this profile's delete/disable/rotation"
        );

        let _started = hold_session_row_marker("work", false, "4242-0").expect("hold a session");
        assert!(
            has_live_session("work"),
            "a real `clauth start` session still reads live with a bare marker present"
        );
    });
}

/// A bare session dies without teardown as the normal case (it never ran clauth
/// code), so its marker file outlives it and only GC removes it.
#[test]
fn gc_prunes_a_dead_bare_session_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let dir = tmp.path().join(".clauth").join("live_bare");
        drop(register_bare_session().expect("register a bare session"));
        assert_eq!(
            fs::read_dir(&dir).expect("read live_bare").count(),
            1,
            "fixture control: a released marker's file stays on disk"
        );

        gc_stale_runtimes();

        assert_eq!(
            fs::read_dir(&dir).expect("read live_bare").count(),
            0,
            "a marker nothing holds must be pruned"
        );
    });
}

#[test]
fn gc_spares_a_held_bare_session_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _bare = register_bare_session().expect("register a bare session");

        gc_stale_runtimes();

        assert_eq!(
            live_bare_sessions(),
            Some(1),
            "GC must not unlink a marker whose session is still running"
        );
    });
}

/// The bare-marker sweep runs at every `clauth mcp` boot, the Plugin tab's
/// 3s-budget probe child included, and the state flock waits up to
/// `STATE_LOCK_TIMEOUT` behind a macOS switch's keychain shell-out. Every other
/// acquisition inside this sweep is conditional on there being work; this one
/// must be too.
#[test]
fn gc_takes_no_state_flock_when_no_bare_marker_exists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
        gc_stale_runtimes();
        assert_eq!(
            crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
            0,
            "a sweep with nothing to collect must not wait on the cross-process lock"
        );

        // Fixture control: with a marker to look at, the sweep DOES lock — or the
        // assertion above would hold against a leg that never runs at all.
        let bare = register_bare_session().expect("register a bare session");
        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
        gc_stale_runtimes();
        assert_eq!(
            crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
            1,
            "the prune itself still runs under the lock, exactly once"
        );

        // The steady state on any box the feature has ever run on, and the arm
        // the first leg does NOT reach: `register_bare_session` mints the dir and
        // the sweep only ever unlinks FILES, so "no bare session running" means an
        // EMPTY dir here, never an absent one. Pinning only the absent case would
        // pin the sweep exactly where it was already free.
        drop(bare);
        let dir = live_bare_dir().expect("bare dir path");
        // The liveness probe is fail-ALIVE, so one transient error can skip a
        // prune; only a persistently-unpruned marker is a regression. Same
        // hardening as `has_live_session_true_when_any_session_alive`, and it
        // keeps a skipped prune from reading as a lock-count failure below.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let emptied = loop {
            gc_stale_runtimes();
            if fs::read_dir(&dir).expect("read live_bare").next().is_none() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            emptied && dir.is_dir(),
            "fixture: the dir must survive the prune, emptied — or this leg degrades into the absent case above"
        );

        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
        gc_stale_runtimes();
        assert_eq!(
            crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
            0,
            "an existing-but-empty marker dir must not wait on the lock either"
        );
    });
}
