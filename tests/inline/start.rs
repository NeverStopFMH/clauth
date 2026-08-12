use super::*;
use std::fs;
use std::process::Command;

use crate::profile::{AppState, Profile};
use crate::runtime::SwapUnsupported;
use crate::testutil::HomeSandbox;

#[cfg(unix)]
fn signal_status(signal: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatus::from_raw(signal)
}

#[test]
fn status_code_preserves_plain_exit_code() {
    let status = Command::new("sh")
        .args(["-c", "exit 7"])
        .status()
        .expect("status");

    assert_eq!(status_code(status, None), 7);
}

#[cfg(unix)]
#[test]
fn status_code_preserves_child_signal_code() {
    assert_eq!(status_code(signal_status(15), None), 143);
}

#[cfg(unix)]
#[test]
fn status_code_reports_parent_signal_after_successful_child_exit() {
    let status = Command::new("sh")
        .args(["-c", "exit 0"])
        .status()
        .expect("status");

    assert_eq!(status_code(status, Some(SIGINT)), 130);
}

// ── apply_spawn_cwd: the resume-cwd primitive (part A) ──

/// `Some(workspace)` pins the child's cwd to that dir — the load-bearing resume
/// guarantee: the resumed `claude` runs in the session's recorded workspace.
#[test]
fn apply_spawn_cwd_pins_child_to_workspace() {
    let mut cmd = Command::new("true");
    let ws = std::path::Path::new("/tmp/clauth-resume-ws");
    let resolved = apply_spawn_cwd(&mut cmd, Some(ws));
    assert_eq!(
        cmd.get_current_dir(),
        Some(ws),
        "the child cwd must equal the recorded workspace"
    );
    assert_eq!(resolved.as_deref(), Some(ws));
}

/// `None` sets no explicit cwd, so the child inherits this process's — byte-for-
/// byte the pre-resume behavior. `get_current_dir` is `None` (unset) in that case.
#[test]
fn apply_spawn_cwd_none_inherits_process_cwd() {
    let mut cmd = Command::new("true");
    let resolved = apply_spawn_cwd(&mut cmd, None);
    assert_eq!(
        cmd.get_current_dir(),
        None,
        "None must not pin the child's cwd"
    );
    assert_eq!(resolved, std::env::current_dir().ok());
}

// ── auto-rescue: the effective-decision + isolated-store teardown ──

/// The pure effective-decision: a per-run `--rescue`/`--no-rescue` override
/// (`Some`) beats the persisted `auto_rescue` toggle; with no override the
/// toggle decides. This is the whole gate `run` composes with `isolation ==
/// Isolated` at teardown.
#[test]
fn rescue_effective_override_beats_toggle() {
    // No per-run flag → the persisted toggle decides.
    assert!(
        !rescue_effective(None, false),
        "default OFF stays off (discard)"
    );
    assert!(rescue_effective(None, true), "toggle ON rescues");
    // A per-run flag overrides the toggle either way.
    assert!(
        !rescue_effective(Some(false), true),
        "--no-rescue beats a true toggle"
    );
    assert!(
        rescue_effective(Some(true), false),
        "--rescue beats a false toggle"
    );
}

/// Bit-identical guard: with rescue OFF (default toggle, no flag) the teardown
/// gate never invokes the store move, so the isolated transcript is left for the
/// runtime GC to discard and the global store stays empty.
#[test]
fn rescue_off_leaves_isolated_store_to_discard() {
    let sb = HomeSandbox::new();
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects");
    let global = sb.home().join(".claude/projects");
    let src = iso.join("-w-iso/s1.jsonl");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "transcript").unwrap();

    // Mirror teardown exactly: decide via the real fn, only move when true.
    let moved = if rescue_effective(None, false) {
        crate::sessions::rescue_isolated_store(&iso, &global)
    } else {
        0
    };

    assert_eq!(moved, 0, "default OFF must not rescue");
    assert!(
        src.exists(),
        "the isolated transcript is left to be discarded"
    );
    assert!(
        !global.join("-w-iso/s1.jsonl").exists(),
        "the global store stays empty — stock discard behavior"
    );
}

/// Rescue ON (via the toggle) lifts the isolated transcript into the global
/// store: it becomes resumable (mirrored `<slug>/<id>.jsonl`) and the source is
/// moved, not copied.
#[test]
fn rescue_on_moves_isolated_transcript_into_global_store() {
    let sb = HomeSandbox::new();
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects");
    let global = sb.home().join(".claude/projects");
    let src = iso.join("-w-iso/s1.jsonl");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "transcript").unwrap();

    let moved = if rescue_effective(None, true) {
        crate::sessions::rescue_isolated_store(&iso, &global)
    } else {
        0
    };

    assert_eq!(moved, 1, "toggle ON rescues the one transcript");
    let landed = global.join("-w-iso/s1.jsonl");
    assert!(
        landed.exists(),
        "the transcript lands in the resumable global store"
    );
    assert_eq!(fs::read(&landed).unwrap(), b"transcript");
    assert!(!src.exists(), "source moved, not copied");
}

/// The gate `run` applies plus the production teardown, so a change to either
/// shows up here. A fresh sessions dir holds one live marker: this session's.
fn teardown(
    rescue_override: Option<bool>,
    auto_rescue: bool,
    iso_root: &std::path::Path,
    claude_home: &std::path::Path,
) -> (usize, usize) {
    if !rescue_effective(rescue_override, auto_rescue) {
        return (0, 0);
    }
    let sessions = iso_root.with_file_name("sessions-isolated");
    fs::create_dir_all(&sessions).unwrap();
    let _self_marker = live_marker(&sessions.join("1234-0"));
    rescue_teardown(iso_root, &sessions, claude_home)
}

/// A live session's liveness marker: an open file holding the same exclusive
/// flock `ProfileRuntime::acquire` takes, so `live_sessions_at` counts it alive
/// (a second fd's `try_lock` conflicts even within one process). The returned
/// handle must stay in scope.
fn live_marker(path: &std::path::Path) -> fs::File {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.lock().unwrap();
    file
}

/// The refcount gate: the isolated runtime tree is SHARED by every session of
/// that profile+flavor (overlapping `delegate`s hold several), and only the last
/// one out sees it discarded. An exit while a sibling is still live must move
/// nothing — rescuing `shell-snapshots/` out from under a running Claude Code
/// would break its Bash tool mid-session.
#[test]
fn rescue_moves_nothing_while_a_sibling_session_is_live() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    let sessions = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(iso.join("shell-snapshots")).unwrap();
    fs::write(iso.join("shell-snapshots/snap.sh"), "live shell").unwrap();
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    let _me = live_marker(&sessions.join("1234-0"));
    let _sibling = live_marker(&sessions.join("5678-0"));

    assert_eq!(
        rescue_teardown(&iso, &sessions, &claude_home),
        (0, 0),
        "a live sibling blocks both legs"
    );
    assert_eq!(
        fs::read_to_string(iso.join("shell-snapshots/snap.sh")).unwrap(),
        "live shell",
        "the live session's state stays where it is reading it"
    );
    assert!(iso.join("projects/-w-iso/s1.jsonl").exists());
    assert!(!claude_home.join("shell-snapshots").exists());
    assert!(!claude_home.join("projects").exists());

    // The sibling exits: the last session out rescues both legs.
    drop(_sibling);
    fs::remove_file(sessions.join("5678-0")).unwrap();
    assert_eq!(rescue_teardown(&iso, &sessions, &claude_home), (1, 1));
}

/// Bit-identical guard, sidecar half: with rescue OFF nothing at all leaves the
/// isolated tree — the sidecars are left for the runtime GC alongside the
/// transcripts.
#[test]
fn rescue_off_leaves_sidecars_to_discard() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(iso.join("shell-snapshots")).unwrap();
    fs::write(iso.join("shell-snapshots/snap.sh"), "iso shell").unwrap();
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();

    assert_eq!(
        teardown(None, false, &iso, &claude_home),
        (0, 0),
        "default OFF rescues neither leg"
    );
    assert!(iso.join("shell-snapshots/snap.sh").exists());
    assert!(
        !claude_home.join("shell-snapshots").exists(),
        "the global store stays untouched — stock discard behavior"
    );
}

/// A sidecar entry that cannot move (its global parent is occupied by a FILE)
/// is logged and skipped: teardown still completes, the rest of the sidecars
/// move, and the transcript leg's result is untouched.
#[test]
fn sidecar_failure_leaves_teardown_and_transcript_rescue_intact() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(iso.join("file-history/sess-a")).unwrap();
    fs::write(iso.join("file-history/sess-a/edit-1.json"), "blocked").unwrap();
    fs::write(iso.join("file-history/ok.json"), "moves").unwrap();
    // A regular file where the rescue needs a directory: every move under it
    // fails at `create_dir_all`.
    fs::create_dir_all(claude_home.join("file-history")).unwrap();
    fs::write(claude_home.join("file-history/sess-a"), "in the way").unwrap();

    let (transcripts, sidecars) = teardown(None, true, &iso, &claude_home);

    assert_eq!(
        (transcripts, sidecars),
        (1, 1),
        "only the blocked entry fails"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl")).unwrap(),
        "transcript",
        "the transcript rescue's result stands"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("file-history/ok.json")).unwrap(),
        "moves"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("file-history/sess-a")).unwrap(),
        "in the way",
        "the blocking file is never replaced"
    );
    assert!(
        iso.join("file-history/sess-a/edit-1.json").exists(),
        "a failed move leaves its source in place, to be discarded"
    );
}

// ── `--with-fallback` eligibility ───────────────────────────────────────────

/// A config eligible for `--with-fallback` in every respect: OAuth, enabled, and
/// a fallback-chain member with somewhere to go. Each refusal below breaks exactly
/// ONE of those and keeps the passing twin, so an assertion cannot pass because
/// the setup was wrong and the call refused for some other reason.
///
/// The chain carries a SECOND member on purpose: a chain whose only entry is this
/// profile is one `walk_chain` can never move off, so building the eligible twin
/// that way would make every positive control a setup that ships the very defect
/// the gates exist to refuse.
fn chain_ready_config(name: &str) -> AppConfig {
    AppConfig {
        state: AppState {
            fallback_chain: vec![name.into(), "spare".into()],
            ..AppState::default()
        },
        profiles: vec![
            Profile::new(name.to_string(), None, None),
            Profile::new("spare".to_string(), None, None),
        ],
    }
}

/// The profile dir a `--with-fallback` start's transport probe would materialize.
/// Nothing else in these tests creates it, so its absence is what proves the probe
/// never ran.
fn profile_dir_of(name: &str) -> std::path::PathBuf {
    crate::profile::profile_dir(name).expect("profile dir")
}

/// Assert the eligible twin cleared every gate under test — the twin each refusal
/// below keeps to prove it refused for the reason it names and not for a broken
/// setup.
///
/// On a host that shares one runtime tree across a profile's sessions (Windows
/// outside Developer Mode, which probes into `LinkMode::Fake`) the twin is refused
/// anyway, by the transport probe. That probe runs LAST, after every gate these
/// tests break one at a time, so reaching it concedes exactly what `Ok` concedes
/// about the gates above — and demanding `Ok` there asserts the right thing about
/// the wrong host. Nothing is weakened where the swap is supported: the probe
/// answers `None` and this is the `expect` it replaced.
///
/// Call it only on the twin, never on a refusal under test: it reads the transport
/// probe, which materializes the profile dir.
#[track_caller]
fn the_eligible_twin_clears_every_gate(verdict: Result<()>, name: &str) {
    match crate::runtime::unsupported_swap_transport(name).expect("probe the transport") {
        None => {
            verdict.expect("the eligible twin must clear every gate");
        }
        Some(why) => assert_eq!(
            verdict
                .expect_err("a shared-tree host refuses the twin at the transport probe")
                .to_string(),
            unsupported_host_refusal(name, why),
            "the twin must reach the transport probe and be refused only there"
        ),
    }
}

/// Both unsupported-host arms' copy, as literals. `cfg!(target_os = "macos")`
/// and `LinkMode::Fake` are each unreachable through the gate from a Linux run,
/// so the render is pinned here and the wiring by the test below it.
#[test]
fn the_unsupported_host_refusal_names_each_cause() {
    assert_eq!(
        unsupported_host_refusal("acme", SwapUnsupported::KeychainFirst),
        "'acme': --with-fallback needs a per-session credential swap, but this host \
         resolves credentials keychain-first; start without it"
    );
    assert_eq!(
        unsupported_host_refusal("acme", SwapUnsupported::SharedRuntimeTree),
        "'acme': --with-fallback needs a per-session credential swap, but this host \
         shares one runtime tree across the profile's sessions; start without it"
    );
}

/// macOS reads credentials Keychain-first and DELETES the plaintext file once it
/// has migrated them, so the swap the flag promises is inert there until the
/// per-config-dir Keychain item is written alongside it. Refused before
/// `acquire`, since the platform is known at compile time.
#[test]
fn with_fallback_refuses_a_keychain_first_host() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let config = chain_ready_config("macish");
    let profile = config.find("macish").expect("fixture profile");

    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared, true)
        .expect_err("a keychain-first host must refuse");
    assert_eq!(
        err.to_string(),
        "'macish': --with-fallback needs a per-session credential swap, but this host \
         resolves credentials keychain-first; start without it"
    );

    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false),
        "macish",
    );
}

/// The freshness gate the decision leg runs reads only the OAuth store, which is
/// sound ONLY because a third-party-launched session gets a chain the walk cannot
/// move it off. Without this refusal such a session opts in and then silently
/// never follows anything.
#[test]
fn with_fallback_refuses_a_non_oauth_profile() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut third_party = chain_ready_config("thirdparty");
    third_party.profiles[0].base_url = Some("https://api.example.com".to_string());
    let profile = third_party.find("thirdparty").expect("fixture profile");

    let err = refuse_unless_chain_eligible(&third_party, profile, Isolation::Shared, false)
        .expect_err("a custom endpoint must refuse");
    assert_eq!(
        err.to_string(),
        "'thirdparty': --with-fallback needs an OAuth account, but this one carries \
         a custom endpoint; start without it"
    );

    let oauth = chain_ready_config("thirdparty");
    let profile = oauth.find("thirdparty").expect("fixture profile");
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&oauth, profile, Isolation::Shared, false),
        "thirdparty",
    );
}

/// A session's chain snapshot returns `None` for a member outside the chain, so
/// the row would be skipped every tick with nothing said. The flag names the fix
/// instead of shipping a silent no-op.
#[test]
fn with_fallback_refuses_a_profile_outside_the_fallback_chain() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut loner = chain_ready_config("loner");
    loner.state.fallback_chain.clear();
    let profile = loner.find("loner").expect("fixture profile");

    let err = refuse_unless_chain_eligible(&loner, profile, Isolation::Shared, false)
        .expect_err("a non-member must refuse");
    assert_eq!(
        err.to_string(),
        "'loner': --with-fallback needs a fallback-chain member; add 'loner' on the \
         fallback tab, or start without it"
    );

    let member = chain_ready_config("loner");
    let profile = member.find("loner").expect("fixture profile");
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&member, profile, Isolation::Shared, false),
        "loner",
    );
}

/// Nothing writes a session's `intended_member` but the daemon's decision leg, so
/// without a daemon the flag is inert and the session sits on its launch account
/// with nothing saying so. Refused rather than auto-spawned: a detached daemon
/// started behind the user's back is a process they never asked for.
#[test]
fn with_fallback_refuses_when_no_daemon_is_running() {
    let _sb = HomeSandbox::new();
    let config = chain_ready_config("undaemoned");
    let profile = config.find("undaemoned").expect("fixture profile");

    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false)
        .expect_err("no daemon must refuse");
    assert_eq!(
        err.to_string(),
        "'undaemoned': --with-fallback needs a running daemon to decide switches, \
         run `clauth daemon`"
    );

    let _daemon = crate::daemon::hold_daemon_lock();
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false),
        "undaemoned",
    );
}

/// `singleton_held` separates "nobody there" from "can't tell" precisely so a
/// decision path can refuse on the second. A host whose lock file cannot be read
/// cannot run the decider on it either, so allowing the flag there ships exactly
/// the silent non-switch the gate exists to prevent.
#[test]
fn with_fallback_refuses_when_the_daemon_lock_cannot_be_read() {
    let _sb = HomeSandbox::new();
    // A directory where the lock file belongs: `open` fails EISDIR, which is the
    // "can't tell" `singleton_held` reports as an error rather than a `no`.
    let lock_path = crate::daemon::daemon_lock_path();
    fs::create_dir_all(&lock_path).expect("mkdir over the lock path");
    let config = chain_ready_config("unreadable");
    let profile = config.find("unreadable").expect("fixture profile");

    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false)
        .expect_err("an unreadable daemon lock must refuse");
    assert_eq!(
        err.to_string(),
        "'unreadable': --with-fallback needs a running daemon and this host could \
         not be checked for one"
    );

    fs::remove_dir(&lock_path).expect("clear the lock path");
    let _daemon = crate::daemon::hold_daemon_lock();
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false),
        "unreadable",
    );
}

/// A chain whose only member is this profile is accepted by a bare membership
/// check and then cannot move: `next_auto_switch_target` has nowhere to point, so
/// the leg writes nothing and the session stays put every tick. Same user-visible
/// silence as the non-member case, which is why it gets the same refusal.
#[test]
fn with_fallback_refuses_a_chain_with_nowhere_to_go() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut lone = chain_ready_config("onlyone");
    lone.state
        .fallback_chain
        .retain(|n| n.as_str() == "onlyone");
    let profile = lone.find("onlyone").expect("fixture profile");

    let err = refuse_unless_chain_eligible(&lone, profile, Isolation::Shared, false)
        .expect_err("a chain of one must refuse");
    assert_eq!(
        err.to_string(),
        "'onlyone': --with-fallback needs a second account in the fallback chain to \
         move to; add one on the fallback tab, or start without it"
    );

    let paired = chain_ready_config("onlyone");
    let profile = paired.find("onlyone").expect("fixture profile");
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&paired, profile, Isolation::Shared, false),
        "onlyone",
    );
}

/// `--isolated` is refused by clap, which is where the user meets it. But `run` is
/// the authoritative chokepoint every session-spawn path inherits, and
/// `chain_opt_in_survives` drops an isolated opt-in SILENTLY — so a new caller
/// passing both would get exactly the no-op the other gates exist to prevent.
#[test]
fn with_fallback_refuses_an_isolated_session() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let config = chain_ready_config("throwaway");
    let profile = config.find("throwaway").expect("fixture profile");

    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Isolated, false)
        .expect_err("an isolated session must refuse");
    assert_eq!(
        err.to_string(),
        "'throwaway': --with-fallback cannot be combined with --isolated, since an \
         isolated session follows no chain"
    );

    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false),
        "throwaway",
    );
}

/// Every gate that can answer without the disk runs BEFORE the transport probe,
/// which is the only leg that writes. So a start refused for a cause the user can
/// act on never materializes a profile dir for an account that never launched —
/// and the compile-time macOS verdict never arrives as a lock timeout or an IO
/// error from a probe it did not need.
#[test]
fn a_refused_with_fallback_start_never_probes_the_disk() {
    let _sb = HomeSandbox::new();
    // No daemon held: the last pure gate refuses.
    let config = chain_ready_config("untouched");
    let profile = config.find("untouched").expect("fixture profile");
    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false)
        .expect_err("no daemon must refuse");
    // WHICH gate refused is the whole subject here: a fixture that drifted into
    // refusing at the oauth or membership gate would leave the dir absent too and
    // stop proving that the LAST pure gate still precedes the probe.
    assert_eq!(
        err.to_string(),
        "'untouched': --with-fallback needs a running daemon to decide switches, \
         run `clauth daemon`"
    );
    assert!(
        !profile_dir_of("untouched").exists(),
        "a refusal the user can act on must not create the profile dir"
    );

    // macOS is known at compile time, so it must not reach the probe either.
    let _daemon = crate::daemon::hold_daemon_lock();
    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared, true)
        .expect_err("a keychain-first host must refuse");
    assert_eq!(
        err.to_string(),
        "'untouched': --with-fallback needs a per-session credential swap, but this host \
         resolves credentials keychain-first; start without it"
    );
    assert!(
        !profile_dir_of("untouched").exists(),
        "a statically-known verdict must not be gated behind a fallible probe"
    );

    // The eligible path DOES probe — otherwise the assertions above pass for a
    // gate that simply never runs the probe at all.
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared, false),
        "untouched",
    );
    assert!(
        profile_dir_of("untouched").is_dir(),
        "the transport probe runs once everything else has cleared"
    );
}

/// The gate has to be WIRED into the one chokepoint every session-spawn path
/// funnels through, and only for a session that asked for the chain. Both halves
/// stop before `claude` is spawned, so the errors are what tells them apart:
/// the opted-in run dies on its own refusal, the bare one gets all the way to
/// `acquire` and dies on the missing `~/.claude` this sandbox has no business
/// creating.
#[test]
fn run_applies_the_chain_gate_only_to_an_opted_in_start() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut loner = chain_ready_config("wired");
    loner.state.fallback_chain.clear();

    let err = run(&loner, "wired", &[], Isolation::Shared, None, None, true)
        .expect_err("an opted-in start must be gated");
    // WHICH gate answers is platform-decided, since `run` passes
    // `cfg!(target_os = "macos")` in and the unsupported-host arm precedes the
    // membership one. Each build sees one arm, so it is the ubuntu and macOS CI
    // legs TOGETHER that reject a hardcoded value: a pinned `false` reds on macOS,
    // a pinned `true` reds everywhere else.
    assert_eq!(
        err.to_string(),
        if cfg!(target_os = "macos") {
            "'wired': --with-fallback needs a per-session credential swap, but this host \
             resolves credentials keychain-first; start without it"
        } else {
            "'wired': --with-fallback needs a fallback-chain member; add 'wired' on the \
             fallback tab, or start without it"
        }
    );

    let err = run(&loner, "wired", &[], Isolation::Shared, None, None, false)
        .expect_err("the sandbox has no ~/.claude to launch against");
    assert_eq!(
        err.to_string(),
        "~/.claude not found; install Claude Code first",
        "a bare start must skip the gate entirely"
    );
}
