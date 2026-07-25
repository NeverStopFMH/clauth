use super::*;
use std::sync::mpsc::channel;

use crate::testutil::HomeSandbox;

/// A row for a session that never ran, with every field pinned so the tests
/// assert exact values rather than "something was written".
fn row(session_id: &str, profile: &str) -> LiveSession {
    LiveSession {
        session_id: session_id.to_string(),
        start_profile: profile.to_string(),
        pid: 4242,
        started_at: 1_700_000_000_000,
        cwd: Some(PathBuf::from("/w/proj")),
        isolated: false,
        follows_chain: false,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
    }
}

/// A row written by a clauth that predates the opt-in field must not read as
/// opted IN on upgrade — the decision leg would then move EVERY live session off
/// the account it launched on.
#[test]
fn a_row_predating_the_opt_in_key_deserializes_as_not_following_the_chain() {
    let pre_upgrade = br#"{"session_id":"4242-0","start_profile":"work","pid":4242,
        "started_at":1700000000000,"cwd":"/w/proj","isolated":false}"#;

    let row: LiveSession = serde_json::from_slice(pre_upgrade).expect("parse a pre-upgrade row");

    assert!(
        !row.follows_chain,
        "a row with no `follows_chain` key must default to opted OUT"
    );
}

#[test]
fn register_then_list_returns_the_row() {
    let _home = HomeSandbox::new();
    let written = row("4242-0", "work");

    register(&written).expect("register");

    assert_eq!(list(), vec![written], "list must round-trip the exact row");
}

#[test]
fn each_writers_update_preserves_the_others_fields() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");

    update_as_daemon("4242-0", |d| {
        d.set_intended_member("kerry");
        d.set_chain_cursor(2);
    })
    .expect("daemon update");
    update_as_session("4242-0", |s| {
        s.set_current_member("work");
        s.set_last_swap_at(1_700_000_009_000);
    })
    .expect("session update");

    let after = list().pop().expect("one row");
    assert_eq!(after.intended_member.as_deref(), Some("kerry"));
    assert_eq!(after.chain_cursor, Some(2));
    assert_eq!(after.current_member.as_deref(), Some("work"));
    assert_eq!(after.last_swap_at, Some(1_700_000_009_000));

    // ...and the other direction: a daemon write after a session write must not
    // drop what the session put there.
    update_as_daemon("4242-0", |d| d.set_intended_member("filip")).expect("second daemon update");

    let after = list().pop().expect("one row");
    assert_eq!(after.intended_member.as_deref(), Some("filip"));
    assert_eq!(
        after.current_member.as_deref(),
        Some("work"),
        "the daemon's write clobbered the session's field"
    );
    assert_eq!(after.last_swap_at, Some(1_700_000_009_000));
    assert_eq!(after.chain_cursor, Some(2));
}

/// THE LOST-UPDATE TEST. The load has to happen INSIDE the state lock, not just
/// the store: a row read before a swap and written after silently reverts
/// whatever the other writer put there in between. Thread A parks inside its
/// closure while holding the lock; B contends. `with_state_lock` serializes them,
/// so B reloads A's stored row and the file must end up carrying BOTH writes.
#[test]
fn a_concurrent_daemon_write_is_not_lost_under_a_parked_session_write() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");

    let (inside_tx, inside_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();

    let session_writer = std::thread::spawn(move || {
        update_as_session("4242-0", |s| {
            inside_tx.send(()).expect("signal inside");
            release_rx.recv().expect("await release");
            s.set_current_member("work");
        })
    });
    inside_rx.recv().expect("A reached its closure");

    let daemon_writer = std::thread::spawn(|| {
        update_as_daemon("4242-0", |d| {
            d.set_intended_member("kerry");
            d.set_chain_cursor(7);
        })
    });
    // B is contending for the lock (or about to be); let it get there before A
    // stores, so a load taken outside the lock would read the pre-A row.
    std::thread::sleep(std::time::Duration::from_millis(300));
    release_tx.send(()).expect("release A");

    session_writer
        .join()
        .expect("session thread panicked")
        .expect("session update");
    daemon_writer
        .join()
        .expect("daemon thread panicked")
        .expect("daemon update");

    let after = list().pop().expect("one row");
    assert_eq!(
        after.current_member.as_deref(),
        Some("work"),
        "the session's write was lost — the daemon stored a row it loaded before it"
    );
    assert_eq!(
        after.intended_member.as_deref(),
        Some("kerry"),
        "the daemon's write was lost — the session stored a row it loaded before it"
    );
    assert_eq!(after.chain_cursor, Some(7));
}

#[test]
fn unregister_removes_the_row_and_is_idempotent() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");
    register(&row("4242-1", "kerry")).expect("register sibling");

    unregister("4242-0").expect("unregister");

    let left: Vec<String> = list().into_iter().map(|r| r.session_id).collect();
    assert_eq!(left, vec!["4242-1".to_string()]);

    unregister("4242-0").expect("a row already gone is not an error");
}

#[test]
fn an_update_of_a_missing_row_names_the_id() {
    let _home = HomeSandbox::new();

    let err = update_as_daemon("4242-9", |d| d.set_chain_cursor(1))
        .expect_err("a missing row must not silently no-op");

    assert!(
        format!("{err:#}").contains("4242-9"),
        "the error must name the id, got: {err:#}"
    );
}

/// A path join must never take a separator or a `..` from an id read back off
/// disk or handed in by a later phase's decision leg.
#[test]
fn a_malformed_session_id_is_refused() {
    let _home = HomeSandbox::new();

    for bad in ["../escape", "4242-0/x", "", "isolated", "4242"] {
        assert!(
            unregister(bad).is_err(),
            "{bad:?} must be refused as a session id"
        );
    }
}
