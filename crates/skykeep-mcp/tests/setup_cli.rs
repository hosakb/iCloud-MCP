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
    assert!(text.contains("No iCloud-MCP folder at"), "{text}");
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
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim_end(),
        "skykeep-mcp: Unknown command, or --state-dir FOLDER is missing. Run skykeep-mcp --help to see the commands."
    );
    let out = program()
        .args(["status", "--password", "x"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: skykeep-mcp status"));
}

#[test]
fn help_lists_every_command_and_exits_zero() {
    for args in [vec!["--help"], vec!["-h"], vec!["help"]] {
        let out = program().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.starts_with("iCloud-MCP"), "{text}");
        assert!(!text.contains("ran without asking"), "{text}");
        assert!(
            text.contains("the rules saved from an approval card"),
            "{text}"
        );
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
            "exchange",
            "permissions show",
            "policy --state-dir FOLDER MODE",
        ] {
            assert!(
                text.contains(&format!("skykeep-mcp {command}")),
                "{args:?} lacks {command}"
            );
        }
    }
}

/// `COMMAND --help` and `help COMMAND` print that command's own help, never the full list.
#[test]
fn each_command_has_its_own_help() {
    for command in [
        "setup",
        "status",
        "unlock",
        "permissions",
        "exchange",
        "login",
        "logout",
        "policy",
        "approve",
        "quarantine",
        "require-approval",
        "allow-routine",
    ] {
        for args in [
            vec![command, "--help"],
            vec![command, "--state-dir", "/nonexistent", "-h"],
            vec!["help", command],
        ] {
            let out = program().args(&args).output().unwrap();
            assert_eq!(out.status.code(), Some(0), "{args:?}");
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(
                text.starts_with(&format!("usage: skykeep-mcp {command}")),
                "{args:?}: {text}"
            );
            let other = if command == "setup" {
                "unlock"
            } else {
                "setup"
            };
            assert!(
                !text.contains(&format!("skykeep-mcp {other} ")),
                "{args:?} shows the full list: {text}"
            );
            assert!(text.lines().count() >= 2, "{args:?}: {text}");
            assert!(!text.contains("ran without asking"), "{args:?}: {text}");
        }
    }
}

/// A command that stops says why in a sentence, not with a bare code.
#[test]
fn terminal_commands_print_a_sentence_not_a_code() {
    let dir = private_dir();
    for args in [
        vec!["quarantine", "--state-dir", dir.path().to_str().unwrap()],
        vec![
            "policy",
            "--state-dir",
            dir.path().to_str().unwrap(),
            "yolo",
        ],
        vec!["login", "--state-dir", dir.path().to_str().unwrap()],
        vec!["logout", "--state-dir", dir.path().to_str().unwrap()],
        vec!["approve", "--state-dir", dir.path().to_str().unwrap()],
    ] {
        let out = program()
            .env("CLAUDECODE", "1")
            .args(&args)
            .output()
            .unwrap();
        assert!(!out.status.success());
        let text = String::from_utf8(out.stderr).unwrap();
        assert!(text.starts_with("skykeep-mcp: "), "{text}");
        assert!(text.contains("your own terminal"), "{args:?}: {text}");
        // The sentence first, then the code in brackets for scripts and searches.
        assert!(
            text.trim_end()
                .ends_with(". (user_owned_terminal_required)"),
            "{text}"
        );
    }
    let out = program().args(["policy", "--state-dir"]).output().unwrap();
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(
        text.starts_with("skykeep-mcp: --state-dir needs a folder after it"),
        "{text}"
    );
    assert!(
        text.trim_end().ends_with("(state_directory_required)"),
        "{text}"
    );
}

/// The policy command explains the mode it sets, not another one.
#[test]
fn the_policy_prompt_explains_the_chosen_mode() {
    let dir = std::path::Path::new("/home/someone/state");
    let strict = skykeep_mcp::terminal::policy_prompt("reads_and_writes", dir);
    assert!(
        strict.contains("Every read and every change asks"),
        "{strict}"
    );
    assert!(!strict.to_lowercase().contains("yolo"), "{strict}");
    assert!(!strict.contains("without asking"), "{strict}");
    let writes = skykeep_mcp::terminal::policy_prompt("writes_only", dir);
    assert!(writes.contains("Reads run without asking"), "{writes}");
    let yolo = skykeep_mcp::terminal::policy_prompt("yolo", dir);
    assert!(
        yolo.contains("Reads and changes run without asking"),
        "{yolo}"
    );
    for text in [strict, writes, yolo] {
        assert!(text.contains("deletions"), "{text}");
        assert!(!text.contains('{'), "no raw JSON: {text}");
    }
}

/// `exchange --path` with a path that is no link says the real reason, not "symbolic link".
#[test]
fn the_exchange_command_gives_the_real_reason_for_an_unusable_path() {
    let state = private_dir();
    let explain = |path: &str| {
        skykeep_mcp::exchange::explain(
            "exchange_folder_path_unsafe",
            Some(std::path::Path::new(path)),
            state.path(),
        )
    };
    let top = explain("/");
    assert!(!top.contains("symbolic link"), "{top}");
    assert!(
        !top.contains("--path /\n") && !top.ends_with("--path /"),
        "{top}"
    );
    assert!(top.contains("~/iCloud-MCP-Exchange"), "{top}");
    for dots in ["/home/x/..", "/home/x/."] {
        let text = explain(dots);
        assert!(!text.contains("symbolic link"), "{text}");
        assert!(text.contains("ends in . or .."), "{text}");
    }
    // A real link still says so, with the folder it points to.
    let target = private_dir();
    let link = state.path().join("link");
    std::os::unix::fs::symlink(target.path(), &link).unwrap();
    let text = explain(link.to_str().unwrap());
    assert!(text.contains("is a symbolic link"), "{text}");
    assert!(
        text.contains(&target.path().canonicalize().unwrap().display().to_string()),
        "{text}"
    );
}
