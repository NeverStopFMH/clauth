//! `clauth autostart install|uninstall|status`: a Windows Task Scheduler
//! entry that runs `clauth daemon` at log on, so the dashboard's backing
//! daemon comes up with no visible terminal window and no manual `clauth
//! daemon` each session.
//!
//! Shells out to `schtasks.exe` rather than pulling in a Task Scheduler
//! crate (`winreg`, `windows`, …) — matching `herdr.rs`'s own precedent of a
//! one-off OS integration being a plain `std::process::Command` call, not a
//! new dependency. `/RL LIMITED` keeps the task non-admin: `clauth` is a
//! per-user tool, so the task should never demand elevation. The daemon's
//! own single-instance flock (`wiki/Daemon.md`) already makes a redundant
//! launch a safe no-op, so a second trigger (a manual `clauth daemon` still
//! running when the scheduled one fires) needs no guard here.
//!
//! **Why a VBScript wrapper, not a direct `/TR` to `clauth.exe`:** a task
//! left at its default "run only when user is logged on" mode (no `/RU`, no
//! stored password) executes INSIDE the user's interactive desktop session —
//! that is what "logged on" means to Task Scheduler — so a console `.exe`
//! launched that way allocates a real, visible console window, identical to
//! double-clicking it. Only "run whether user is logged on or not" (Session
//! 0, non-interactive) truly has no desktop to show a window on, and that
//! mode needs a stored Windows credential (`/RP`), which is more standing
//! secret-management than a background dashboard warrants. The fix is the
//! well-established one: point the task at `wscript.exe` running a tiny
//! generated script that calls `WScript.Shell.Run(cmd, 0, False)` — window
//! style `0` = hidden — to launch `clauth.exe daemon` itself with no window
//! at all, because `wscript.exe` is a GUI-subsystem host with no console of
//! its own to inherit or spawn.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::out::{errln, out, outln};
use crate::profile::{clauth_dir, mkdir_700};

/// Task Scheduler name. Prefixed so it reads unambiguously in Task
/// Scheduler's flat namespace instead of a bare "daemon".
const TASK_NAME: &str = "clauth-daemon";
/// The generated hidden-launch script's filename, under `~/.clauth/`.
const LAUNCHER_FILE: &str = "autostart_launch.vbs";

fn is_tty() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Default-no, matching `herdr::confirm`: both install (registers a task
/// that runs indefinitely, every login) and uninstall (removes it) are
/// standing changes to the user's system outside anything clauth already
/// owns.
fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_tty() {
        errln!("clauth: not a terminal, so nothing was changed; rerun with --yes");
        return Ok(false);
    }
    out!("clauth: {question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

fn require_windows() -> Result<()> {
    if !cfg!(windows) {
        bail!("clauth autostart is Windows-only (Task Scheduler); nothing to do on this platform");
    }
    Ok(())
}

fn launcher_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(LAUNCHER_FILE))
}

/// The VBScript that launches `exe_path daemon` with window style `0`
/// (hidden). Split out from [`install`] so the generated source is
/// unit-testable without touching the filesystem. `exe_path` is doubled-quote
/// escaped per VBScript string-literal rules (a literal `"` inside a
/// VBScript string is written `""`) — always a no-op for a real Windows
/// path, but exact rather than assumed.
fn launcher_script(exe_path: &str) -> String {
    let escaped = exe_path.replace('"', "\"\"");
    format!(
        "Set shell = CreateObject(\"WScript.Shell\")\r\n\
         shell.Run \"\"\"{escaped}\"\" daemon\", 0, False\r\n"
    )
}

/// The `schtasks /Create` argument list — split out from [`install`] so the
/// command-line shape is unit-testable without actually invoking
/// `schtasks.exe`. `vbs_path` is the generated launcher script's absolute
/// path; `//B` runs `wscript.exe` in batch mode (no error/prompt dialogs).
fn create_args(vbs_path: &str) -> Vec<String> {
    vec![
        "/Create".to_string(),
        "/TN".to_string(),
        TASK_NAME.to_string(),
        "/TR".to_string(),
        format!("wscript.exe //B \"{vbs_path}\""),
        "/SC".to_string(),
        "ONLOGON".to_string(),
        "/RL".to_string(),
        "LIMITED".to_string(),
        "/F".to_string(),
    ]
}

pub(crate) fn install(yes: bool) -> Result<()> {
    require_windows()?;
    let exe = std::env::current_exe().context("failed to resolve clauth's own executable path")?;
    let exe = exe.to_string_lossy().into_owned();

    if !confirm(
        &format!("register a task that runs '{exe}' daemon at every log on (no visible window)?"),
        yes,
    )? {
        return Ok(());
    }

    let dir = clauth_dir()?;
    mkdir_700(&dir).context("failed to create ~/.clauth")?;
    let vbs_path = launcher_path()?;
    std::fs::write(&vbs_path, launcher_script(&exe))
        .with_context(|| format!("failed to write {}", vbs_path.display()))?;
    let vbs_path = vbs_path.to_string_lossy().into_owned();

    let status = Command::new("schtasks")
        .args(create_args(&vbs_path))
        .status()
        .context("failed to run schtasks.exe")?;
    if !status.success() {
        bail!("schtasks.exe /Create exited with {status}");
    }
    outln!(
        "clauth: autostart installed — '{TASK_NAME}' runs 'clauth daemon' at every log on, no window"
    );
    Ok(())
}

pub(crate) fn uninstall(yes: bool) -> Result<()> {
    require_windows()?;
    if !confirm(&format!("remove the '{TASK_NAME}' scheduled task?"), yes)? {
        return Ok(());
    }

    let status = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .status()
        .context("failed to run schtasks.exe")?;
    if !status.success() {
        bail!("schtasks.exe /Delete exited with {status} (was it installed?)");
    }
    // Best-effort: the task is already gone at this point either way, and a
    // leftover launcher script with no task pointing at it is inert, so a
    // failed removal here (already absent, permissions) is not worth failing
    // the whole uninstall over.
    if let Ok(path) = launcher_path() {
        let _ = std::fs::remove_file(path);
    }
    outln!("clauth: autostart removed");
    Ok(())
}

pub(crate) fn status() -> Result<()> {
    require_windows()?;
    let output = Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .output()
        .context("failed to run schtasks.exe")?;
    if output.status.success() {
        outln!("clauth: autostart is installed ('{TASK_NAME}')");
    } else {
        outln!("clauth: autostart is not installed");
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/autostart.rs"]
mod tests;
