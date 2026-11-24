use icloud_mcp::Server;
use quince_core::notes::{NoteFolder, TRASH_FOLDER};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

fn setup() -> (tempfile::TempDir, Server, quince_core::Library) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    library
        .put_note_folders(&[
            NoteFolder {
                guid: "DefaultFolder-CloudKit".into(),
                title: "Notes".into(),
                sort_order: 0,
                parent_guid: None,
            },
            NoteFolder {
                guid: TRASH_FOLDER.into(),
                title: "Recently Deleted".into(),
                sort_order: 1,
                parent_guid: None,
            },
        ])
        .unwrap();
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

/// Initializes as a client that shows the approval card.
async fn init_cards(server: &mut Server) {
    server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
}

/// The raw reply to one call, for a card.
async fn raw(server: &mut Server, name: &str, args: Value) -> Value {
    server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap()
}

async fn accept(server: &mut Server, form: &Value) -> Value {
    let response = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
        .await
        .unwrap();
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn create_note(server: &mut Server, body: &str) -> String {
    let created = call(
        server,
        "notes_write",
        json!({"action":"create","body":body,"folder_guid":"DefaultFolder-CloudKit"}),
    )
    .await;
    created["guid"].as_str().expect("created").to_string()
}

#[tokio::test]
async fn moving_a_note_into_the_trash_asks_on_the_card() {
    let (_dir, mut server, library) = setup();
    init_cards(&mut server).await;
    let guid = create_note(&mut server, "Synthetic\nkeep me").await;
    let queued = library.outbox_count().unwrap();
    let form = raw(
        &mut server,
        "notes_write",
        json!({"action":"move","guid":guid,"folder_guid":TRASH_FOLDER,"base_etag":null}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert!(!form.to_string().contains("always_allow"), "{form}");
    assert_eq!(library.outbox_count().unwrap(), queued);
    assert_eq!(
        library.note(&guid).unwrap().unwrap().folder_guid.as_deref(),
        Some("DefaultFolder-CloudKit")
    );
    let approved = accept(&mut server, &form).await;
    assert_eq!(approved["status"], "queued", "{approved}");
    assert_eq!(
        library.note(&guid).unwrap().unwrap().folder_guid.as_deref(),
        Some(TRASH_FOLDER)
    );
}

#[tokio::test]
async fn creating_a_note_in_the_trash_is_refused() {
    let (_dir, mut server, library) = setup();
    let r = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"x","folder_guid":TRASH_FOLDER}),
    )
    .await;
    assert_eq!(r["error"], "folder_not_writable", "{r}");
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn creating_a_note_in_a_missing_folder_returns_folder_not_found() {
    let (_dir, mut server, library) = setup();
    let r = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"x","folder_guid":"does-not-exist"}),
    )
    .await;
    assert_eq!(r["error"], "folder_not_found", "{r}");
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn saving_an_empty_body_asks_on_the_card() {
    let (_dir, mut server, library) = setup();
    init_cards(&mut server).await;
    let guid = create_note(&mut server, "Synthetic\nkeep me").await;
    let queued = library.outbox_count().unwrap();
    for body in ["", "  \n\t"] {
        let form = raw(
            &mut server,
            "notes_write",
            json!({"action":"save","guid":guid,"base_etag":null,"body":body}),
        )
        .await;
        assert_eq!(form["method"], "elicitation/create", "{body:?}: {form}");
        // Closed without a choice: Deny, and nothing runs.
        let reply = server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
            .await
            .unwrap();
        assert!(reply.to_string().contains("approval_denied"), "{reply}");
    }
    assert_eq!(library.outbox_count().unwrap(), queued);
    assert_eq!(
        library.note_body(&guid).unwrap().as_deref(),
        Some("Synthetic\nkeep me")
    );
    // A non-empty save stays routine.
    let r = call(
        &mut server,
        "notes_write",
        json!({"action":"save","guid":guid,"base_etag":null,"body":"Synthetic\nstill here"}),
    )
    .await;
    assert_eq!(r["status"], "queued", "{r}");
}

#[tokio::test]
async fn batch_request_gets_an_invalid_request_error() {
    let (_dir, mut server, _library) = setup();
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let r = server
        .handle(json!([{"jsonrpc":"2.0","id":3,"method":"ping"}]))
        .await
        .expect("a batch must not be dropped silently");
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], Value::Null);
    assert_eq!(r["error"]["code"], -32600, "{r}");
    // Notifications still get no response.
    assert!(server
        .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await
        .is_none());
}

#[tokio::test]
async fn list_tool_schemas_match_their_arguments() {
    let (_dir, mut server, _library) = setup();
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let r = server
        .handle(json!({"id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = r["result"]["tools"].as_array().unwrap();
    let schema = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name}"))["inputSchema"]
            .clone()
    };
    assert_eq!(schema("reminders_list")["required"], json!(["parent"]));
    assert!(schema("notes_list")["properties"]["parent"].is_object());
    for name in ["notes_folders", "reminders_lists"] {
        assert!(schema(name)["properties"].get("parent").is_none(), "{name}");
        let r = call(&mut server, name, json!({"parent":"zzz"})).await;
        assert_eq!(r["error"], "invalid_arguments", "{name}: {r}");
    }
}
