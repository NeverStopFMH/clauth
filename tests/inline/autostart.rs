//! Pure tests for the generated VBScript launcher and the `schtasks.exe`
//! argument shape — never actually invokes `schtasks.exe` or writes a file
//! (that would register/query a real Task Scheduler entry, or touch the real
//! `~/.clauth`, on whatever machine runs the suite).

use super::*;

#[test]
fn launcher_script_runs_the_exe_hidden_with_the_daemon_subcommand() {
    let script = launcher_script(r"C:\Program Files\clauth\clauth.exe");
    assert!(
        script.contains(r#"shell.Run """C:\Program Files\clauth\clauth.exe"" daemon", 0, False"#)
    );
}

#[test]
fn launcher_script_escapes_an_embedded_quote() {
    // Not a real Windows path, but exercises the escape rule rather than
    // assuming a path never contains one.
    let script = launcher_script(r#"C:\weird"path\clauth.exe"#);
    assert!(script.contains(r#"C:\weird""path\clauth.exe"#));
}

#[test]
fn create_args_targets_wscript_with_the_launcher_script() {
    let args = create_args(r"C:\Users\alice\.clauth\autostart_launch.vbs");
    assert_eq!(
        args,
        vec![
            "/Create",
            "/TN",
            "clauth-daemon",
            "/TR",
            r#"wscript.exe //B "C:\Users\alice\.clauth\autostart_launch.vbs""#,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
        ]
    );
}

#[test]
fn task_name_has_no_spaces_or_reserved_task_scheduler_characters() {
    assert!(
        TASK_NAME
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    );
}
