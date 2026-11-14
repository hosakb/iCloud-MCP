use serde_json::{json, Value};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;

async fn call(server: &mut Server, name: &str, args: Value) -> Value {
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let r = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn sync_status_reports_queue_without_network_or_raw_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    call(
        &mut server,
        "notes_write",
        json!({"action":"create","body":"Queued synthetic"}),
    )
    .await;
    library
        .mark_sync_failed("notes", "HTTP 500 SYNTHETIC-RAW-BODY")
        .unwrap();
    let status = call(&mut server, "sync_status", json!({})).await;
    let notes = status["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"] == "notes")
        .unwrap()
        .clone();
    assert_eq!(notes["pending"], 1, "{status}");
    assert_eq!(notes["refused"], 0);
    assert_eq!(notes["last_error"], true);
    assert!(status["services"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["service"] == "reminders"));
    assert_eq!(
        status["services"].as_array().unwrap().len(),
        2,
        "only notes and reminders"
    );
    assert!(!status.to_string().contains("SYNTHETIC-RAW-BODY"));
    assert_eq!(
        call(&mut server, "sync_status", json!({"x":1})).await["error"],
        "invalid_arguments"
    );
}
