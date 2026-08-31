//! `load_or_create_token`: generated once, persisted, stable across repeat
//! calls (a bookmarked `clauth web url` link must keep working). `check_bearer`:
//! the pure 401 gate, exercised without any real HTTP round trip.

use super::*;

#[test]
fn creates_a_64_char_hex_token_on_first_call() {
    let _home = crate::testutil::HomeSandbox::new();
    let token = load_or_create_token().expect("first token");
    assert_eq!(token.len(), 64, "32 bytes hex-encoded");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn repeat_calls_return_the_same_token() {
    let _home = crate::testutil::HomeSandbox::new();
    let first = load_or_create_token().expect("first token");
    let second = load_or_create_token().expect("second token");
    assert_eq!(first, second, "a bookmarked link must keep working");
}

#[cfg(unix)]
#[test]
fn token_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let _home = crate::testutil::HomeSandbox::new();
    load_or_create_token().expect("token");
    let path = token_path().expect("token path");
    let mode = std::fs::metadata(&path)
        .expect("token file metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn check_bearer_accepts_only_the_exact_token() {
    assert!(check_bearer(Some("Bearer abc123"), "abc123"));
    assert!(!check_bearer(Some("Bearer wrong"), "abc123"));
    assert!(
        !check_bearer(Some("abc123"), "abc123"),
        "missing 'Bearer ' prefix"
    );
    assert!(!check_bearer(None, "abc123"));
}
