use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

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
            String::from_utf8_lossy(&output.stderr).contains("interactive_terminal_required"),
            "{command}: {:?}",
            output.stderr
        );
    }
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive_terminal_required"));
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("user_owned_terminal_required"));
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
    assert_eq!(read["source"], "local_cache");
    assert_eq!(read["items"], json!([]));
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
