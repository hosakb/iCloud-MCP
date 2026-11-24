use icloud_mcp::Server;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

fn setup() -> (tempfile::TempDir, Server, quince_core::Library) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    (dir, server, library)
}
async fn call(server: &mut Server, name: &str, args: Value) -> Value {
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let response = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
/// The call on a client that shows the approval card: the card comes first, nothing runs
/// before it is approved, and the approved call's result is returned.
async fn approved_on_card(server: &mut Server, name: &str, args: Value) -> Value {
    server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
    let form = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert!(!form.to_string().contains("request_id"), "{form}");
    let response = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
        .await
        .unwrap();
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
#[tokio::test]
async fn reminder_create_uses_durable_upstream_queue() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            title: "Synthetic".into(),
            ..Default::default()
        }])
        .unwrap();
    let result = call(&mut server, "reminders_write", json!({"action":"create","list_guid":"list","title":"Synthetic task","due_at":123456,"flagged":true})).await;
    assert_eq!(result["status"], "queued", "{result}");
    let guid = result["guid"].as_str().unwrap();
    let item = library.reminder(guid).unwrap().unwrap();
    assert_eq!(item.title, "Synthetic task");
    assert_eq!(item.due_at, Some(123456));
    assert!(item.flagged);
    assert_eq!(library.outbox_pending("reminders").unwrap().len(), 1);
    assert_eq!(result["remote_verified"], false);
}
#[tokio::test]
async fn reminder_creation_respects_admin_approval() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    let result = approved_on_card(
        &mut server,
        "reminders_write",
        json!({"action":"create","list_guid":"list","title":"Synthetic"}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 1);
}
#[tokio::test]
async fn reminder_get_includes_native_details() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminder_local(&quince_core::reminders::Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "Synthetic task".into(),
            ..Default::default()
        })
        .unwrap();
    let result = call(&mut server, "reminders_get", json!({"guid":"r"})).await;
    assert_eq!(result["item"]["title"], "Synthetic task", "{result}");
    assert_eq!(result["complete_representation"], false);
}
#[tokio::test]
async fn reminder_edit_preserves_absent_fields_and_clears_null() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminder_local(&quince_core::reminders::Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "Before".into(),
            notes: Some("Keep".into()),
            due_at: Some(1234),
            ..Default::default()
        })
        .unwrap();
    let result = call(
        &mut server,
        "reminders_write",
        json!({"action":"edit","guid":"r","patch":{"title":"After","due_at":null}}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    let item = library.reminder("r").unwrap().unwrap();
    assert_eq!(item.title, "After");
    assert_eq!(item.notes.as_deref(), Some("Keep"));
    assert_eq!(item.due_at, None);
}
#[tokio::test]
async fn reminder_tick_uses_upstream_completion_semantics() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminder_local(&quince_core::reminders::Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            ..Default::default()
        })
        .unwrap();
    let result = call(
        &mut server,
        "reminders_write",
        json!({"action":"tick","guid":"r","done":true}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert!(library
        .reminder("r")
        .unwrap()
        .unwrap()
        .completed_at
        .is_some());
    assert_eq!(library.outbox_pending("reminders").unwrap()[0].kind, "done");
}
#[tokio::test]
async fn reminder_delete_asks_on_the_card() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminder_local(&quince_core::reminders::Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            ..Default::default()
        })
        .unwrap();
    let result = approved_on_card(
        &mut server,
        "reminders_write",
        json!({"action":"delete","guid":"r"}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert!(library.reminder("r").unwrap().is_none());
}
#[tokio::test]
async fn note_save_reuses_native_base_text_and_runs() {
    let (_dir, mut server, library) = setup();
    let created = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Title\nBefore"}),
    )
    .await;
    let guid = created["guid"].as_str().unwrap();
    let result = call(
        &mut server,
        "notes_write",
        json!({"action":"save","guid":guid,"base_etag":null,"body":"Title\nAfter"}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(
        library.note_body(guid).unwrap().as_deref(),
        Some("Title\nAfter")
    );
    let pending = library.outbox_pending("notes").unwrap();
    let save: Value =
        serde_json::from_str(&pending.iter().find(|p| p.kind == "save").unwrap().payload).unwrap();
    assert_eq!(save["base"], "Title\nBefore");
}
#[tokio::test]
async fn note_pin_records_upstream_mutation() {
    let (_dir, mut server, library) = setup();
    let created = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Title"}),
    )
    .await;
    let guid = created["guid"].as_str().unwrap();
    let result = call(
        &mut server,
        "notes_write",
        json!({"action":"pin","guid":guid,"pinned":true,"base_etag":null}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert!(library.note(guid).unwrap().unwrap().pinned);
}
#[tokio::test]
async fn note_move_records_native_folder() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_note_folders(&[quince_core::notes::NoteFolder {
            guid: "folder".into(),
            title: "Synthetic".into(),
            sort_order: 0,
            parent_guid: None,
        }])
        .unwrap();
    let created = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Title"}),
    )
    .await;
    let guid = created["guid"].as_str().unwrap();
    let result = call(
        &mut server,
        "notes_write",
        json!({"action":"move","guid":guid,"folder_guid":"folder","base_etag":null}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(
        library.note(guid).unwrap().unwrap().folder_guid.as_deref(),
        Some("folder")
    );
}
#[tokio::test]
async fn tool_discovery_advertises_each_implemented_operation() {
    let (_dir, mut server, _) = setup();
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let result = server
        .handle(json!({"id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = result["result"]["tools"].as_array().unwrap();
    for name in [
        "notes_folders",
        "reminders_lists",
        "reminders_get",
        "reminders_write",
    ] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    let note = tools.iter().find(|t| t["name"] == "notes_write").unwrap();
    assert!(note["inputSchema"]["properties"]["action"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("save")));
}
#[tokio::test]
async fn notes_search_uses_local_index() {
    let (_dir, mut server, _) = setup();
    call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Distinctive synthetic title"}),
    )
    .await;
    let result = call(&mut server, "notes_search", json!({"query":"Distinctive"})).await;
    assert_eq!(
        result["items"].as_array().map(Vec::len),
        Some(1),
        "{result}"
    );
}
#[tokio::test]
async fn reminder_search_uses_local_index() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            enabled: true,
            ..Default::default()
        }])
        .unwrap();
    call(
        &mut server,
        "reminders_write",
        json!({"action":"create","list_guid":"list","title":"Distinctive synthetic task"}),
    )
    .await;
    let result = call(
        &mut server,
        "reminders_search",
        json!({"query":"Distinctive"}),
    )
    .await;
    assert_eq!(
        result["items"].as_array().map(Vec::len),
        Some(1),
        "{result}"
    );
}
#[tokio::test]
async fn note_get_exposes_runs_without_lossy_markdown() {
    let (_dir, mut server, mut library) = setup();
    let created = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Title"}),
    )
    .await;
    let guid = created["guid"].as_str().unwrap();
    library.set_note_runs(guid, "[]").unwrap();
    let result = call(&mut server, "notes_get", json!({"guid":guid})).await;
    assert_eq!(result["runs"], json!([]), "{result}");
}
#[tokio::test]
async fn a_running_client_reopens_new_account_binding_without_restart() {
    let (dir, mut server, mut library) = setup();
    let held = icloud_mcp::state::PrivateState::open(dir.path()).unwrap();
    let blocked = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Blocked"}),
    )
    .await;
    assert_eq!(blocked["error"], "state_busy_retry");
    library
        .set_state("account", "replacement-synthetic-account")
        .unwrap();
    drop(held);
    let result = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"After helper"}),
    )
    .await;
    assert_eq!(result["status"], "queued");
    assert_eq!(
        library.state("account").unwrap().as_deref(),
        Some("replacement-synthetic-account")
    );
    assert_eq!(library.outbox_count().unwrap(), 1);
}
#[tokio::test]
async fn background_refresh_returns_login_required_without_network() {
    let (_dir, mut server, mut library) = setup();
    library.set_state("account", "").unwrap();
    // A blank synthetic binding cannot produce a resumable protected session.
    assert!(server.refresh().await.is_err());
}
#[tokio::test]
async fn folder_discovery_reads_upstream_cache() {
    let (_dir, mut server, library) = setup();
    let result = call(&mut server, "notes_folders", json!({})).await;
    assert_eq!(result["items"], json!(library.note_folders().unwrap()));
    assert_eq!(result["source"], "local_cache");
}
#[tokio::test]
async fn reminder_list_discovery_reads_upstream_cache() {
    let (_dir, mut server, library) = setup();
    let result = call(&mut server, "reminders_lists", json!({})).await;
    assert_eq!(result["items"], json!(library.reminder_lists().unwrap()));
}
#[tokio::test]
async fn reminder_list_discovery_reports_pin_dates() {
    let (_dir, mut server, mut library) = setup();
    let list =
        |guid: &str, pinned: bool, pinned_at: Option<i64>| quince_core::reminders::ReminderList {
            guid: guid.into(),
            title: "Synthetic".into(),
            pinned,
            pinned_at,
            ..Default::default()
        };
    library
        .put_reminder_lists(&[
            list("dated", true, Some(1_790_609_001_389)),
            list("undated", true, None),
            list("plain", false, None),
        ])
        .unwrap();
    let result = call(&mut server, "reminders_lists", json!({})).await;
    let items = result["items"].as_array().unwrap();
    let item = |guid: &str| items.iter().find(|i| i["guid"] == guid).unwrap().clone();
    assert_eq!(item("dated")["pinned"], true);
    assert_eq!(item("dated")["pinnedAt"], 1_790_609_001_389_i64);
    assert_eq!(item("undated")["pinned"], true);
    assert_eq!(item("undated")["pinnedAt"], Value::Null);
    assert_eq!(item("plain")["pinned"], false);
    assert_eq!(item("plain")["pinnedAt"], Value::Null);
    assert!(items.iter().all(|i| i.get("reminderOrder").is_none()));
}
