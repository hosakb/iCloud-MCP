//! Permissions with the real program: `permissions show` from outside, and the server as
//! Hermes 0.23.3 starts it. Kept apart from the in-process tests, so no child process can
//! hold a folder lock another test is waiting for.
use icloud_mcp::permissions;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn write_rules(dir: &Path, text: &str) {
    let path = dir.join(permissions::FILE_NAME);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn body(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn choices(form: &Value) -> Vec<String> {
    form["params"]["requestedSchema"]["properties"]["decision"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["const"].as_str().unwrap().to_string())
        .collect()
}

fn create_reminder() -> Value {
    json!({"action":"create","list_guid":"list","title":"Synthetic created"})
}

#[test]
fn permissions_show_runs_from_any_terminal_and_never_asks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    drop(quince_core::Library::open(dir.path()).unwrap());
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        std::fs::set_permissions(
            entry.unwrap().path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    write_rules(dir.path(), "rules:\n  notes_list: deny\n");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_icloud-mcp"))
        .args(["permissions", "show", "--state-dir"])
        .arg(dir.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("notes_list: deny, is always refused"),
        "{text}"
    );
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_icloud-mcp"))
        .args(["permissions", "apply", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "there is no apply step");
}

/// The server as Hermes 0.23.3 starts it: this exact initialize, then one ask: client call.
fn hermes(decision: Value) -> (tempfile::TempDir, Value, Value) {
    use std::io::{BufRead, BufReader, Write};
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        library.set_state("account", "synthetic").unwrap();
        library
            .put_reminder_lists(&[quince_core::reminders::ReminderList {
                guid: "list".into(),
                title: "Synthetic".into(),
                ..Default::default()
            }])
            .unwrap();
    }
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        std::fs::set_permissions(
            entry.unwrap().path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    write_rules(
        dir.path(),
        "rules:\n  reminders_write.create:\n    ask: client\n",
    );
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_icloud-mcp"))
        .args(["--state-dir"])
        .arg(dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut next = || -> Value { serde_json::from_str(&output.next().unwrap().unwrap()).unwrap() };
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"hermes","version":"0.23.3"}}})).unwrap();
    next();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"reminders_write","arguments":create_reminder()}})).unwrap();
    let form = next();
    let mut reply = decision;
    reply["id"] = form["id"].clone();
    writeln!(input, "{reply}").unwrap();
    let result = next();
    drop(input);
    child.wait().unwrap();
    (dir, form, result)
}

#[test]
fn hermes_shows_the_card_and_approve_runs_the_action() {
    let (dir, form, result) = hermes(
        json!({"jsonrpc":"2.0","result":{"action":"accept","content":{"decision":"approve"}}}),
    );
    assert_eq!(form["method"], "elicitation/create");
    assert_eq!(form["params"]["mode"], "form");
    assert_eq!(choices(&form), ["approve", "deny", "always_allow"]);
    assert_eq!(body(&result)["status"], "queued", "{result}");
    let library = quince_core::Library::open(dir.path()).unwrap();
    assert_eq!(library.outbox_count().unwrap(), 1);
}

#[test]
fn hermes_declining_the_card_runs_nothing() {
    for decline in [
        json!({"jsonrpc":"2.0","result":{"action":"decline"}}),
        json!({"jsonrpc":"2.0","result":{"action":"accept","content":{"decision":"deny"}}}),
    ] {
        let (dir, form, result) = hermes(decline);
        assert_eq!(form["method"], "elicitation/create");
        assert_eq!(body(&result)["error"], "approval_denied", "{result}");
        let library = quince_core::Library::open(dir.path()).unwrap();
        assert_eq!(library.outbox_count().unwrap(), 0);
    }
}
