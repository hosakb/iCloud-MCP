use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn negotiated_form_approves_exact_create_before_enqueue() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic").unwrap();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            title: "Synthetic".into(),
            ..Default::default()
        }])
        .unwrap();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        std::fs::set_permissions(
            entry.unwrap().path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    skykeep_mcp::Server::open(dir.path())
        .unwrap()
        .set_policy("writes_only")
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"))
        .args(["--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    writeln!(input, "{}", json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}}}}})).unwrap();
    let _: Value = serde_json::from_str(&output.next().unwrap().unwrap()).unwrap();
    writeln!(input, "{}", json!({"id":2,"method":"tools/call","params":{"name":"reminders_write","arguments":{"action":"create","list_guid":"list","title":"exact synthetic operation"}}})).unwrap();
    let form: Value = serde_json::from_str(&output.next().unwrap().unwrap()).unwrap();
    assert_eq!(form["method"], "elicitation/create");
    assert_eq!(
        form["params"]["requestedSchema"],
        json!({"type":"object","properties":{"decision":{"type":"string","title":"Decision","oneOf":[{"const":"approve","title":"Approve this time"},{"const":"deny","title":"Deny"},{"const":"always_allow","title":"Always allow this action"}]}},"required":["decision"]})
    );
    assert_eq!(library.outbox_count().unwrap(), 0);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}})
    )
    .unwrap();
    let reply: Value = serde_json::from_str(&output.next().unwrap().unwrap()).unwrap();
    assert_eq!(reply["id"], 2);
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(library.outbox_count().unwrap(), 1);
    drop(input);
    assert!(child.wait().unwrap().success());
}

/// Subprocesses must not inherit markers from an agent shell running the tests.
fn clean() -> Vec<(&'static str, &'static str)> {
    skykeep_mcp::terminal::AGENT_MARKERS
        .iter()
        .map(|n| (*n, ""))
        .collect()
}

#[test]
fn login_requires_a_controlling_terminal_and_never_reads_pipe() {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"))
        .args(["login", "--state-dir"])
        .arg(dir.path())
        .envs(clean())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    for command in ["approve", "require-approval", "allow-routine"] {
        let output = Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"))
            .args([command, "--state-dir"])
            .arg(dir.path())
            .envs(clean())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("needs a real terminal"),
            "{command}: {:?}",
            output.stderr
        );
    }
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs a real terminal"));
}

#[test]
fn agent_marked_process_cannot_open_a_secret_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"))
        .args(["login", "--state-dir"])
        .arg(dir.path())
        .env("HERMES_AGENT", "true")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("AI agent's terminal"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let policy = Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"))
        .args(["policy", "--state-dir"])
        .arg(dir.path())
        .arg("yolo")
        .env("HERMES_AGENT", "true")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!policy.status.success());
    assert!(String::from_utf8_lossy(&policy.stderr).contains("AI agent's terminal"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn stdio_initializes_lists_and_reads_without_stdout_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("skykeep-mcp");
    assert!(
        binary.is_file(),
        "the local stdio entry point must be built"
    );
    let mut child = Command::new(binary)
        .arg("--state-dir")
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"stdio-test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"notes_list","arguments":{}}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "skykeep-mcp");
    assert!(responses[1]["result"]["tools"].as_array().unwrap().len() >= 2);
    let read: Value = serde_json::from_str(
        responses[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    // A folder without a sign-in says so instead of an empty list.
    assert_eq!(read["error"], "not_signed_in", "{read}");
}

/// Spawning tests share this binary only with other spawning tests: a forked child briefly
/// holds copies of the parent's descriptors, including directory locks.
#[test]
fn stdio_server_answers_initialize_while_a_peer_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let _held = skykeep_mcp::state::PrivateState::open(dir.path()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"))
        .arg("--state-dir")
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"busy-test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"notes_list","arguments":{}}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(responses.len(), 2, "{responses:?}");
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "skykeep-mcp");
    let text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap()["error"],
        "state_busy_retry"
    );
}
