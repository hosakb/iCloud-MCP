//! Own test binary: it points the Secret Service client at a socket that does not exist.
use serde_json::{json, Value};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn unavailable_secure_storage_returns_login_required_without_prompt() {
    // Never contact a real keyring from tests.
    unsafe {
        std::env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/skykeep-test-bus",
        );
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", "synthetic-account")
        .unwrap();
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let started = std::time::Instant::now();
    let r = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":"sync","arguments":{}}}))
        .await
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    let result: Value =
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .starts_with("login_required"),
        "{result}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "must fail fast, not wait on an unlock dialog"
    );
}
