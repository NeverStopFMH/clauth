//! Shared test-only helpers used across the inline test modules
//! (`tests/inline/*.rs`). Defined once here rather than copied per module so the
//! home-sandbox, mtime, and key-event scaffolding stays in a single place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// RAII home sandbox: acquires `HOME_TEST_LOCK` and redirects `home_dir()` into
/// a tempdir for its lifetime, clearing the override on drop (even on panic).
/// Required for any test that writes into the per-profile tree or creates
/// session dirs, pid files, or rotation locks — otherwise those paths land in
/// the real `~/.clauth`.
pub(crate) struct HomeSandbox {
    // Drop order: tempdir first, then the shared lock.
    _tmp: tempfile::TempDir,
    _guard: crate::lockorder::RankedGuard<'static, ()>,
    home: PathBuf,
}

impl HomeSandbox {
    pub(crate) fn new() -> Self {
        // Untracked HOME_TEST_LOCK acquired first; no RankedMutex/flock is held.
        let guard = crate::profile::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("create home sandbox");
        let home = tmp.path().to_path_buf();
        crate::profile::set_home_override(home.clone());
        Self {
            _tmp: tmp,
            _guard: guard,
            home,
        }
    }

    /// Path to the sandboxed home directory.
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for HomeSandbox {
    fn drop(&mut self) {
        // Join BEFORE clearing the override, not after and not per-test. A
        // detached worker still running when `HOME_OVERRIDE` clears resolves
        // the operator's REAL `$HOME` and takes real locks under `~/.clauth`
        // (`RotationGuard::acquire` alone does `mkdir_700` + a blocking
        // flock). Doing it here rather than asking each test to call the join
        // fns covers the tests that never thought about it — which is every
        // test that will ever be added. Two registries: the TUI's own
        // `spawn_worker` handles (joinable OS threads) and
        // [`join_background_tasks`] for detach mechanisms that hand back no
        // joinable handle at all (e.g. `tokio::task::spawn_blocking`).
        crate::tui::join_test_workers();
        join_background_tasks();
        crate::profile::clear_home_override();
    }
}

/// Completion signals for detached background tasks that have no joinable
/// handle of their own — e.g. the MCP background delegate, which detaches via
/// `tokio::task::spawn_blocking` and drops the returned task handle
/// immediately (see `mcp::launch_background_delegate`). Hoisted here rather
/// than living beside `tui::TEST_WORKERS` because a second subsystem now
/// needs the same join: this is the shared test-helper home, not
/// TUI-specific. Never compiled into the binary.
#[cfg(test)]
static BACKGROUND_TASK_DONE: std::sync::Mutex<Vec<std::sync::mpsc::Receiver<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Register a detached task's completion receiver so [`HomeSandbox::drop`]
/// can block on it before it clears the home override. The returned sender is
/// the task's contract: send on it as the LAST action, after every
/// `$HOME`-touching step (config load, disk write) is done. A guard bound
/// inside the task drops in reverse declaration order and therefore lands
/// AFTER the send, so any guard whose `Drop` reaches `$HOME` has to be dropped
/// explicitly before it.
#[cfg(test)]
pub(crate) fn register_background_task() -> std::sync::mpsc::Sender<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Ok(mut done) = BACKGROUND_TASK_DONE.lock() {
        done.push(rx);
    }
    tx
}

/// Block until every task registered via [`register_background_task`] has
/// signaled completion.
#[cfg(test)]
fn join_background_tasks() {
    let pending: Vec<_> = BACKGROUND_TASK_DONE
        .lock()
        .map(|mut d| std::mem::take(&mut *d))
        .unwrap_or_default();
    for rx in pending {
        let _ = rx.recv();
    }
}

// ── printable-escape-hatch probes ────────────────────────────────────────────
//
// The error types that hold upstream-derived facts (`oauth::TokenFailure`,
// `RefreshError`, `KickError`, `oauth_login::AuthorizeRejection`) are contained
// by NOT being printable: no `Display` means `{e}` does not compile, and no
// `Into<anyhow::Error>` means `?` cannot launder one into something that does.
// Both properties are invisible to a normal assertion, so probe them: the
// inherent method wins method lookup whenever its bound holds, and lookup falls
// through to the blanket trait method when it does not.

pub(crate) struct Probe<T>(std::marker::PhantomData<T>);

impl<T: std::fmt::Display> Probe<T> {
    pub(crate) fn is_display() -> bool {
        true
    }
}

impl<T: Into<anyhow::Error>> Probe<T> {
    pub(crate) fn into_anyhow() -> bool {
        true
    }
}

pub(crate) trait NotDisplay {
    fn is_display() -> bool {
        false
    }
}

impl<T> NotDisplay for Probe<T> {}

pub(crate) trait NotIntoAnyhow {
    fn into_anyhow() -> bool {
        false
    }
}

impl<T> NotIntoAnyhow for Probe<T> {}

impl<T: Send> Probe<T> {
    pub(crate) fn is_send() -> bool {
        true
    }
}

pub(crate) trait NotSend {
    fn is_send() -> bool {
        false
    }
}

impl<T> NotSend for Probe<T> {}

// ── offline rotation-leg harness ─────────────────────────────────────────────
//
// Every rotation decision sits BEHIND an HTTP call, so a refusal deleted from
// `fetch_with_rotation`, `auto_start_kick` or `rotate_one_inner` is invisible to
// any test that cannot answer that call. These live here rather than beside one
// test module because both the scheduler and the oauth suites drive those legs.

/// A loopback stand-in for the Anthropic hosts, answering by request PATH so a
/// leg's request ORDER isn't baked into the fixture. Serves up to `max` requests
/// and returns the path of each one it saw, in order.
///
/// `max` must be set ABOVE what a correct run makes, never equal to it. A
/// must-NOT-call assertion (`!seen.contains(token_endpoint)`) is only meaningful
/// if the listener would have accepted and recorded that call — a `max` sized to
/// the happy path makes the forbidden request invisible and the assertion passes
/// no matter what the code does. That exact fixture bug let a deleted refusal
/// stay green here once already.
///
/// The listener is NON-BLOCKING with two deadlines. A leg that refuses early
/// makes fewer requests than `max`, and a blocking `accept` would hang the suite
/// instead of failing it — the shape a restored refusal has, so the harness
/// would swallow the very mutation it exists to catch. `IDLE_GRACE` is what
/// bounds the "nothing more is coming" case, and must stay above the 5s per-host
/// request spacing or a paced follow-up reads as absent.
pub(crate) fn serve_endpoints(
    max: usize,
    reply: impl Fn(&str, usize) -> (u16, String) + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// Long enough for a leg that sleeps on pacing before its FIRST request.
    const FIRST_WAIT: Duration = Duration::from_secs(45);
    /// Above `REQUEST_SPACING_MS` (5s) plus the kick's 2s step delay.
    const IDLE_GRACE: Duration = Duration::from_secs(12);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let handle = std::thread::spawn(move || {
        let mut seen: Vec<String> = Vec::new();
        for i in 0..max {
            let deadline = Instant::now()
                + if seen.is_empty() {
                    FIRST_WAIT
                } else {
                    IDLE_GRACE
                };
            let mut sock = loop {
                if Instant::now() > deadline {
                    return seen;
                }
                match listener.accept() {
                    Ok((sock, _)) => break sock,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return seen,
                }
            };
            sock.set_nonblocking(false).ok();
            sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            // Drain headers AND any Content-Length body before replying: a
            // close with unread bytes RSTs the client on Windows.
            let mut req = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        req.extend_from_slice(&tmp[..n]);
                        if let Some(h) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                            let len = String::from_utf8_lossy(&req[..h])
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if req.len() >= h + 4 + len {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&req).into_owned();
            let path = text
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let (status, body) = reply(&path, i);
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = sock.write_all(body.as_bytes());
            let _ = sock.shutdown(std::net::Shutdown::Write);
            seen.push(path);
        }
        seen
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

pub(crate) fn rotation_fixture_config(name: &str) -> crate::profile::ConfigHandle {
    let mut profile = blank_profile(name);
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-old".into(),
            refresh_token: Some("rt-old".into()),
            // Far outside the lead window, so the leg is REACTIVE: the 401 is
            // what drives it, not the proactive predicate.
            expires_at: Some(crate::usage::now_ms() as i64 + 86_400_000),
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    config.state.active_profile = Some(name.into());
    std::sync::Arc::new(crate::lockorder::RankedMutex::new(config))
}

/// Make the next `save_profile` for `name` fail on its credentials write, so a
/// rotation's persist leg can be driven without root or a mode flip: the write
/// goes through `atomic_write_600`, whose `rename(tmp, credentials.json)` is
/// `EISDIR` once a DIRECTORY sits at that path.
///
/// Aimed at `credentials.json` rather than the profile dir on purpose — a broken
/// profile dir fails `RotationGuard::acquire` first (`rotation.lock` lives there),
/// so the leg would bail long before reaching the persist under test.
///
/// Gated with its callers: both are non-macOS tests, and an ungated helper with
/// no macOS caller is a dead-code error that reds that leg on clippy
/// `-D warnings` before a test runs.
#[cfg(not(target_os = "macos"))]
pub(crate) fn block_credentials_write(name: &str) {
    let path = crate::profile::profile_subpath(name, "credentials.json").expect("credentials path");
    if path.is_file() {
        std::fs::remove_file(&path).expect("drop the fixture's credentials file");
    }
    std::fs::create_dir(&path).expect("block the credentials path with a directory");
}

/// RAII pin redirecting every Anthropic endpoint the rotation legs touch —
/// `/usage`, the token endpoint, and the `/v1/messages` kick — at one loopback
/// listener, and clearing them on drop even if the test panics. Also resets the
/// per-host request spacing, or the second request in a leg sleeps out
/// `REQUEST_SPACING_MS`, and the adopt's token → uuid memo, which is
/// process-lifetime: a fake token two tests share would otherwise let the first
/// answer the second's probe and delete the very request it asserts on.
///
/// Rotation decisions all sit BEHIND an HTTP call, so without this the refusals
/// in `fetch_with_rotation` / `auto_start_kick` / `rotate_one_inner` are covered
/// by nothing.
///
/// It BORROWS the [`HomeSandbox`] rather than documenting "outlive me": the
/// overrides are process-globals serialized by `HOME_TEST_LOCK`, which the home
/// sandbox holds, so dropping the home first would release that lock while the
/// overrides are still set and let the next test run against them. As a borrow
/// that inversion is E0505 at compile time instead of a race nothing checks.
pub(crate) struct EndpointSandbox<'a>(std::marker::PhantomData<&'a HomeSandbox>);

impl<'a> EndpointSandbox<'a> {
    /// Point every endpoint at `base` (an `http://127.0.0.1:PORT` listener).
    pub(crate) fn new(_home: &'a HomeSandbox, base: &str) -> Self {
        crate::oauth::set_endpoint_overrides(
            &format!("{base}/v1/oauth/token"),
            &format!("{base}/v1/messages?beta=true"),
        );
        crate::usage::set_usage_endpoint_override(
            &format!("{base}/api/oauth/usage"),
            &format!("{base}/api/oauth/profile"),
        );
        crate::usage::reset_request_slots();
        crate::usage::reset_identity_memo();
        crate::oauth::reset_stored_probe_suppression();
        Self(std::marker::PhantomData)
    }
}

impl Drop for EndpointSandbox<'_> {
    fn drop(&mut self) {
        crate::oauth::clear_endpoint_overrides();
        crate::usage::clear_usage_endpoint_override();
        crate::usage::reset_request_slots();
        crate::usage::reset_identity_memo();
        crate::oauth::reset_stored_probe_suppression();
    }
}

/// RAII `CLAUDE_CONFIG_DIR` pin: forces the var for its lifetime and restores the
/// previous value on drop (even on panic). Required by any test exercising a path
/// that reads the session's config dir — `which::session_auth`,
/// `which::resolve_active`, and everything attributing loaded credentials.
///
/// It BORROWS the [`HomeSandbox`] for the same reason [`EndpointSandbox`] does:
/// the env is a process-global serialized by `HOME_TEST_LOCK`, which the home
/// sandbox holds, so dropping the home first would release that lock with this
/// pin still standing and let the next test run against it. As a borrow that
/// inversion is E0505 at compile time instead of a race nothing checks.
pub(crate) struct ConfigDirSandbox<'a> {
    prev: Option<std::ffi::OsString>,
    _home: std::marker::PhantomData<&'a HomeSandbox>,
}

impl<'a> ConfigDirSandbox<'a> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    pub(crate) fn new(_home: &'a HomeSandbox, dir: &Path) -> Self {
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        // SAFETY: test-only, serialized by `HOME_TEST_LOCK`, restored on drop.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", dir) };
        Self {
            prev,
            _home: std::marker::PhantomData,
        }
    }
}

impl Drop for ConfigDirSandbox<'_> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    fn drop(&mut self) {
        // SAFETY: same as `new` — restore the prior value under the same lock.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
        }
    }
}

/// RAII tier pin: acquires `TIER_TEST_LOCK` and forces the process-global color
/// tier for its lifetime, putting the previous pin back on drop (even on panic).
/// Required for any test asserting on a tier-dependent style, since the tier is
/// process-global and otherwise auto-detects from the ambient `$COLORTERM`.
pub(crate) struct TierSandbox {
    // Drop order: this type's `drop` restores under the lock, which the field
    // then releases.
    _guard: crate::lockorder::RankedGuard<'static, ()>,
    prev: Option<crate::tui::theme::Tier>,
}

impl TierSandbox {
    pub(crate) fn new(tier: crate::tui::theme::Tier) -> Self {
        let guard = crate::tui::theme::TIER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = crate::tui::theme::tier_override();
        crate::tui::theme::set_tier(tier);
        Self {
            _guard: guard,
            prev,
        }
    }
}

impl Drop for TierSandbox {
    fn drop(&mut self) {
        crate::tui::theme::restore_tier(self.prev);
    }
}

/// A minimal `Profile` with every optional field unset — tests fill in what
/// they assert on.
pub(crate) fn blank_profile(name: &str) -> crate::profile::Profile {
    crate::profile::Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: Default::default(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

// ── provider-cache fixtures ──────────────────────────────────────────────────
//
// A `third_party_cache.json` in each of the two SHAPES the provider legs really
// write. Both carry the complete key set, row set and row `kind`s read off an
// operator's own caches on 2026-08-15 (a DeepSeek balance profile and a z.ai bar
// profile, `serde_json` key-shape only); every number, amount and reset stamp is
// substituted, so these are shaped-from-a-capture rather than captured bytes,
// and no account figure is committed.
//
// Kept as BYTES rather than a serialized `ThirdPartyStats`: a struct built in
// Rust agrees with whatever the reader guessed, while these go through the
// production reader like every real consumer does.

/// The balance shape: `rows` only, no `bars`, and no `plan` key at all. Its
/// wallet row carries the CAPTURED `total`, which is also the label every cache
/// an older clauth wrote still holds on disk today, and the one the generic
/// scanner still passes an endpoint's own key through as. Consumers must keep
/// reading it — [`DEEPSEEK_CACHE_BYTES`] is the same shape at the current
/// spelling, and the two together are what hold both halves of that.
pub(crate) const THIRD_PARTY_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"CNY balance","value":"","kind":"heading"},{"label":"total","value":"31.45 CNY","kind":"body"},{"label":"granted","value":"0.00 CNY","kind":"body"},{"label":"topped up","value":"31.45 CNY","kind":"body"}],"bars":[],"best_effort":false}"#;

/// [`THIRD_PARTY_CACHE_BYTES`] as the DeepSeek leg writes it now: same capture,
/// same key set, wallet row at [`crate::providers::DEEPSEEK_BALANCE_ROW_LABEL`].
/// For a test asserting what a DeepSeek account renders TODAY; the constant
/// above is what it renders off a cache written before the rename.
pub(crate) const DEEPSEEK_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"CNY balance","value":"","kind":"heading"},{"label":"api balance","value":"31.45 CNY","kind":"body"},{"label":"granted","value":"0.00 CNY","kind":"body"},{"label":"topped up","value":"31.45 CNY","kind":"body"}],"bars":[],"best_effort":false}"#;

/// The third shape, and the one a bar-count reader gets wrong: a provider that
/// PUBLISHES usage windows answering with none of them. `alibaba::window_bar`
/// drops a window whose percentage the response omitted and both are optional,
/// so this is what a qwen account caches when neither arrived — plan and
/// subscription rows, an empty `bars`, and no wallet anywhere.
pub(crate) const ALIBABA_NO_BARS_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"subscription","value":"","kind":"heading"},{"label":"status","value":"valid","kind":"body"},{"label":"remaining","value":"84 days","kind":"body"}],"bars":[],"plan":"coding plan","best_effort":false}"#;

/// The bar shape: three `bars` under a `plan` label, of which only the longest
/// window carries `used`/`total`, plus the section-headed row set a token
/// provider writes. The mixed bar keys are the point — a reader that assumed
/// every bar carries the same five fields parses this one wrong.
pub(crate) const THIRD_PARTY_BARS_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"30d","value":"","kind":"heading"},{"label":"search-prime","value":"12 / 100","kind":"body"},{"label":"web-reader","value":"3 / 100","kind":"body"},{"label":"zread","value":"0 / 50","kind":"body"},{"label":"7d tokens","value":"","kind":"heading"},{"label":"GLM-5.3","value":"80.1M","kind":"body"},{"label":"GLM-5.2","value":"40.2M","kind":"body"},{"label":"GLM-4.7","value":"3.1M","kind":"body"},{"label":"total","value":"123.4M  (1.2k calls)","kind":"faint"}],"bars":[{"label":"5h","pct":12.5,"resets_at":"2026-08-15T12:00:00Z"},{"label":"7d","pct":48.0,"resets_at":"2026-08-20T00:00:00Z"},{"label":"30d","pct":3.0,"resets_at":"2026-09-01T00:00:00Z","used":123.4,"total":4000.0}],"plan":"pro","best_effort":false}"#;

/// Overwrite a file's modification time — for cache-staleness / tie-break tests.
pub(crate) fn set_mtime(path: &Path, when: SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_modified(when).expect("set_modified");
}

/// A `Press` key event with no modifiers.
pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Collect a `Command`'s queued env overrides: key → `Some(value)` for a set
/// var, key → `None` for a removed one. `get_envs` reflects only the explicit
/// `env`/`env_remove` ops, which is exactly what we assert. No process env or
/// spawn needed, so this is lock-free and non-flaky.
pub(crate) fn env_overrides(cmd: &Command) -> HashMap<String, Option<String>> {
    cmd.get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|s| s.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

/// Every path under `root` breaking the owner-only invariant clauth holds over
/// `~/.clauth` (0o700 dirs, 0o600 files), rendered as `<mode> <path>` lines.
/// Symlinks are skipped — a link's own mode is meaningless and its target lives
/// outside the tree.
#[cfg(unix)]
pub(crate) fn owner_only_violations(root: &Path) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;

    let mut out = Vec::new();
    let Ok(meta) = root.symlink_metadata() else {
        return out;
    };
    if meta.file_type().is_symlink() {
        return out;
    }
    let is_dir = meta.file_type().is_dir();
    let mode = meta.permissions().mode() & 0o777;
    let want = if is_dir { 0o700 } else { 0o600 };
    if mode != want {
        out.push(format!("{mode:#o} {} (want {want:#o})", root.display()));
    }
    if is_dir && let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            out.extend(owner_only_violations(&entry.path()));
        }
    }
    out
}

/// Flatten a rendered `TestBackend` buffer to one `String` per row (cell symbols
/// concatenated). Shared by the TUI render tests so each keeps a single copy of
/// the buffer→text step; callers `.concat()` or `.join("\n")` to taste.
pub(crate) fn buffer_rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    let w = buf.area.width as usize;
    let h = buf.area.height as usize;
    (0..h)
        .map(|y| (0..w).map(|x| buf.content[y * w + x].symbol()).collect())
        .collect()
}
