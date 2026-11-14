use serde_json::json;
use skykeep_mcp::Server;

#[tokio::test]
async fn initialize_and_discover_only_notes_and_reminders_tools() {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut server = Server::open(dir.path()).unwrap();
    let init = server.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "skykeep-mcp");
    assert!(server
        .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await
        .is_none());
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"notes_list"));
    assert!(names.contains(&"reminders_list"));
    assert!(!names.iter().any(|n| [
        "photos", "drive", "calendar", "contacts", "secret", "password", "code"
    ]
    .iter()
    .any(|s| n.contains(s))));
    assert!(server
        .handle(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}})
        )
        .await
        .is_none());
}
