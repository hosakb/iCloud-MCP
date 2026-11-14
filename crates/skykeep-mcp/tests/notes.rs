use quince_core::Library;
use serde_json::{json, Value};
use skykeep_mcp::Server;

async fn call(server: &mut Server, name: &str, args: Value) -> Value {
    server
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .await;
    server.handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}})).await.unwrap()
}

#[tokio::test]
async fn note_create_queues_in_upstream_outbox_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    {
        let mut l = Library::open(dir.path()).unwrap();
        l.set_state(quince_sync::queue::ACCOUNT_KEY, "synthetic-account")
            .unwrap();
    }
    for file in std::fs::read_dir(dir.path()).unwrap() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.unwrap().path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    let mut server = Server::open(dir.path()).unwrap();
    let created = call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Synthetic title\nSynthetic body"}),
    )
    .await;
    assert_eq!(created["result"]["isError"], false, "{created}");
    let result: Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "queued");
    assert_eq!(result["remote_verified"], false);
    let fetched = call(&mut server, "notes_get", json!({"guid":result["guid"]})).await;
    assert_eq!(fetched["result"]["isError"], false, "{fetched}");
    let fetched: Value =
        serde_json::from_str(fetched["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(fetched["item"]["body"], "Synthetic title\nSynthetic body");
    let guid = result["guid"].as_str().unwrap();
    drop(server);
    let library = Library::open(dir.path()).unwrap();
    assert_eq!(
        library.note_body(guid).unwrap().as_deref(),
        Some("Synthetic title\nSynthetic body")
    );
    assert_eq!(library.outbox_pending("notes").unwrap().len(), 1);
    for file in std::fs::read_dir(dir.path()).unwrap() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.unwrap().path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    let mut server = Server::open(dir.path()).unwrap();
    let response = call(&mut server, "notes_list", json!({})).await;
    let value: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["items"][0]["guid"], guid);
    assert_eq!(value["source"], "local_cache");
}
