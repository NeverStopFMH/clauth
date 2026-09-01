//! Pure tests for the `schtasks.exe` argument shape — never actually invokes
//! `schtasks.exe` (that would register/query a real Task Scheduler entry on
//! whatever machine runs the suite).

use super::*;

#[test]
fn create_args_quotes_the_exe_path_and_targets_the_daemon_subcommand() {
    let args = create_args(r"C:\Program Files\clauth\clauth.exe");
    assert_eq!(
        args,
        vec![
            "/Create",
            "/TN",
            "clauth-daemon",
            "/TR",
            r#""C:\Program Files\clauth\clauth.exe" daemon"#,
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
