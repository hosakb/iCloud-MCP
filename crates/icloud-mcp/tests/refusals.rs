//! A Notes or Reminders write that is refused keeps its own error code in every approval
//! mode, the same reply it gets with no mode. Synthetic data only, no network.
use icloud_mcp::Server;
use quince_core::notes::Note;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

/// A private folder with one note at etag-1 and one reminder list, in `mode`, after an
/// initialize that offers approval cards.
async fn folder(mode: Option<&str>) -> (tempfile::TempDir, Server, quince_core::Library) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    library
        .put_notes(&[Note {
            guid: "note".into(),
            folder_guid: None,
            title: "Synthetic".into(),
            snippet: None,
            body: Some("Synthetic\nBody".into()),
            locked: false,
            pinned: false,
            created_at: None,
            modified_at: Some(1),
            editable: true,
            etag: Some("etag-1".into()),
            runs: None,
        }])
        .unwrap();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            title: "Synthetic".into(),
            ..Default::default()
        }])
        .unwrap();
    if let Some(mode) = mode {
        server.set_policy(mode).unwrap();
    }
    server
        .handle(json!({"id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}}}}}))
        .await;
    (dir, server, library)
}

/// The tool's reply, after approving each card the mode shows.
async fn call(server: &mut Server, name: &str, args: Value) -> Value {
    let mut reply = server
        .handle(json!({"id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    while reply["method"] == "elicitation/create" {
        reply = server
            .handle(json!({"jsonrpc":"2.0","id":reply["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
            .await
            .unwrap();
    }
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn refusals_keep_their_own_code_in_every_approval_mode() {
    let cases = [
        (
            "notes_write",
            json!({"action":"pin","guid":"note","pinned":true}),
            "conflict",
        ),
        (
            "reminders_write",
            json!({"action":"create","list_guid":"list","title":"Synthetic","repeat":{"frequency":"daily"}}),
            "repeat_requires_due_at",
        ),
    ];
    for (name, args, code) in cases {
        let (_dir, mut server, library) = folder(None).await;
        let expected = call(&mut server, name, args.clone()).await;
        assert_eq!(expected["error"], code, "{expected}");
        if code == "conflict" {
            assert!(expected["instructions"]
                .as_str()
                .is_some_and(|t| t.contains("base_etag")));
        }
        drop(library);
        for mode in ["writes_only", "reads_and_writes", "yolo"] {
            let (_dir, mut server, library) = folder(Some(mode)).await;
            let reply = call(&mut server, name, args.clone()).await;
            assert_eq!(reply, expected, "{name} under {mode}");
            assert_eq!(library.outbox_count().unwrap(), 0, "{name} under {mode}");
        }
    }
}
