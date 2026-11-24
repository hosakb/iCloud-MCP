use icloud_mcp::Server;
use serde_json::{json, Value};
async fn call(server: &mut Server, name: &str, args: Value) -> Value {
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let r = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
#[tokio::test]
async fn status_explains_user_terminal_login_without_touching_a_keyring() {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut s = Server::open(dir.path()).unwrap();
    let result = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(result["status"], "login_required");
    assert!(result["instructions"]
        .as_str()
        .unwrap()
        .contains("own trusted terminal"));
    let sync = call(&mut s, "sync", json!({})).await;
    assert_eq!(sync["error"], "login_required");
    let forbidden = call(&mut s, "auth_status", json!({"password":"SYNTHETIC"})).await;
    assert_eq!(forbidden["error"], "invalid_arguments");
}
