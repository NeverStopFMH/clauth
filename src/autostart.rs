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

use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::out::{errln, out, outln};

/// Task Scheduler name. Prefixed so it reads unambiguously in Task
/// Scheduler's flat namespace instead of a bare "daemon".
const TASK_NAME: &str = "clauth-daemon";

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

/// The `/Create` argument list — split out from [`install`] so the
/// command-line shape is unit-testable without actually invoking
/// `schtasks.exe`. `exe_path` is `clauth.exe`'s own absolute path
/// (`std::env::current_exe`), quoted since it can contain spaces
/// (`Program Files`) and Task Scheduler's `/TR` takes one string.
fn create_args(exe_path: &str) -> Vec<String> {
    vec![
        "/Create".to_string(),
        "/TN".to_string(),
        TASK_NAME.to_string(),
        "/TR".to_string(),
        format!("\"{exe_path}\" daemon"),
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
        &format!("register a task that runs '{exe}' daemon at every log on?"),
        yes,
    )? {
        return Ok(());
    }

    let status = Command::new("schtasks")
        .args(create_args(&exe))
        .status()
        .context("failed to run schtasks.exe")?;
    if !status.success() {
        bail!("schtasks.exe /Create exited with {status}");
    }
    outln!("clauth: autostart installed — '{TASK_NAME}' runs 'clauth daemon' at every log on");
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
