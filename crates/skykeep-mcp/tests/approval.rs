use serde_json::{json, Value};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn admin_all_write_policy_requires_terminal_approval() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let r = server.handle(json!({"id":2,"method":"tools/call","params":{"name":"notes_write","arguments":{"action":"create","body":"Synthetic approval note"}}})).await.unwrap();
    let result: Value =
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "approval_required");
    assert_eq!(library.outbox_count().unwrap(), 0);
    let id = result["request_id"].as_str().unwrap();
    let approved = server.approve(id).unwrap();
    assert_eq!(approved["status"], "queued");
    assert_eq!(library.outbox_count().unwrap(), 1);
    assert!(server.approve(id).is_err());
    library
        .set_state("mcp.require_write_approval", "false")
        .unwrap();
    let guid = approved["guid"].as_str().unwrap();
    let r = server.handle(json!({"id":3,"method":"tools/call","params":{"name":"notes_write","arguments":{"action":"delete","guid":guid,"base_etag":null}}})).await.unwrap();
    let result: Value =
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "approval_required");
    assert!(library.note(guid).unwrap().is_some());
    assert_eq!(
        server
            .approve(result["request_id"].as_str().unwrap())
            .unwrap()["status"],
        "queued"
    );
    assert_eq!(library.outbox_count().unwrap(), 2);
}
