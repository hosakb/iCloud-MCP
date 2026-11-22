use serde_json::{json, Value};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;

fn accept(form: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}})
}

fn body(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A folder without an approval mode, set earlier to approve every change: each change asks
/// on the card, never with a terminal request, and runs once when approved.
#[tokio::test]
async fn admin_all_write_policy_asks_on_the_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
    let form = server.handle(json!({"id":2,"method":"tools/call","params":{"name":"notes_write","arguments":{"action":"create","body":"Synthetic approval note"}}})).await.unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert!(!form.to_string().contains("request_id"), "{form}");
    assert_eq!(library.outbox_count().unwrap(), 0);
    let reply = server.handle(accept(&form)).await.unwrap();
    let approved = body(&reply);
    assert_eq!(approved["status"], "queued", "{approved}");
    assert_eq!(library.outbox_count().unwrap(), 1);
    // The same answer again is no approval: nothing runs twice.
    assert!(server.handle(accept(&form)).await.is_none());
    assert_eq!(library.outbox_count().unwrap(), 1);
    library
        .set_state("mcp.require_write_approval", "false")
        .unwrap();
    let guid = approved["guid"].as_str().unwrap();
    let form = server.handle(json!({"id":3,"method":"tools/call","params":{"name":"notes_write","arguments":{"action":"delete","guid":guid,"base_etag":null}}})).await.unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    // A deletion offers no "Always allow".
    assert!(!form.to_string().contains("always_allow"), "{form}");
    assert!(library.note(guid).unwrap().is_some());
    let reply = server.handle(accept(&form)).await.unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
    assert_eq!(library.outbox_count().unwrap(), 2);
}
