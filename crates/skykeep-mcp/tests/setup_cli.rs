//! The real program: setup, unlock and status from outside, with no terminal and no keyring.
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

fn program() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"));
    // Never reach a real keyring from tests.
    c.env(
        "DBUS_SESSION_BUS_ADDRESS",
        "unix:path=/nonexistent/skykeep-test-bus",
    )
    .stdin(Stdio::null());
    for marker in skykeep_mcp::terminal::AGENT_MARKERS {
        c.env_remove(marker);
    }
    c
}

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[test]
fn setup_and_unlock_refuse_without_a_real_terminal_in_one_sentence() {
    let dir = private_dir();
    for (command, start) in [
        ("setup", "setup needs a real terminal"),
        ("unlock", "unlock needs a real terminal"),
    ] {
        let out = program()
            .args([command, "--state-dir"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success());
        let text = String::from_utf8(out.stderr).unwrap();
        assert!(text.starts_with(&format!("skykeep-mcp: {start}")), "{text}");
        assert_eq!(text.trim_end().lines().count(), 1, "{text}");
        assert!(out.stdout.is_empty());
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "nothing written"
    );
}

#[test]
fn setup_refuses_in_an_agent_terminal() {
    let dir = private_dir();
    let out = program()
        .env("CLAUDECODE", "1")
        .args(["setup", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("AI agent's terminal"));
}

#[test]
fn status_reports_a_missing_folder_and_exits_non_zero_without_creating_it() {
    let parent = private_dir();
    let missing = parent.path().join("not-there");
    let out = program()
        .args(["status", "--state-dir"])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("No Skykeep folder at"), "{text}");
    assert!(
        text.contains(&format!(
            "Next step: Run skykeep-mcp setup --state-dir {}.",
            missing.display()
        )),
        "{text}"
    );
    assert!(!missing.exists());
}

#[test]
fn status_on_a_fresh_folder_lists_each_problem_with_a_next_step() {
    let dir = private_dir();
    let out = program()
        .args(["status", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("OK   Folder:"), "{text}");
    assert!(text.contains("system keyring, not reachable"), "{text}");
    assert!(text.contains("No approval mode chosen."), "{text}");
    assert!(text.matches("Next step:").count() >= 2, "{text}");
}

#[test]
fn status_names_an_open_folder_and_the_fix() {
    let dir = private_dir();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = program()
        .args(["status", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("is not private to you"), "{text}");
    assert!(text.contains("chmod 700"), "{text}");
}

#[test]
fn unlock_rejects_bad_hours_and_older_commands_keep_their_usage() {
    let out = program().args(["unlock", "--hours", "0"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("from 1 to 8760"));
    let out = program().args(["login"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
    let out = program()
        .args(["status", "--password", "x"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: skykeep-mcp status"));
}

#[test]
fn help_lists_every_command_and_exits_zero() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["setup", "--help"],
        vec!["unlock", "--help"],
        vec!["status", "--help"],
        vec!["login", "--help"],
        vec!["logout", "--state-dir", "/nonexistent", "--help"],
        vec!["approve", "--help"],
        vec!["policy", "--help"],
        vec!["quarantine", "--help"],
        vec!["require-approval", "--help"],
    ] {
        let out = program().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        for command in [
            "setup",
            "status",
            "unlock",
            "login",
            "logout",
            "policy",
            "approve",
            "quarantine",
            "require-approval",
            "allow-routine",
            "policy --state-dir FOLDER MODE",
        ] {
            assert!(
                text.contains(&format!("skykeep-mcp {command}")),
                "{args:?} lacks {command}"
            );
        }
    }
}
