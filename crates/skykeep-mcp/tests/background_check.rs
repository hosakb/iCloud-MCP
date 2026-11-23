//! Own test binary: it installs a storage environment for the whole process. The automatic
//! background sync records whether the sign-in worked, like the sync tool does. A folder
//! store with no session stands in for the keyring; nothing reaches a network.
use serde_json::{json, Value};
use skykeep_mcp::keyhold::MemoryHolder;
use skykeep_mcp::storage::{self, Env, Kind};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[tokio::test]
async fn the_background_sync_records_a_failed_sign_in() {
    unsafe {
        libc::umask(0o077);
    }
    storage::use_env(Env {
        keyring: Box::new(|dir: &Path| quince_auth::Store::file(dir)),
        holder: Box::new(MemoryHolder::default()),
    })
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    {
        let held = skykeep_mcp::state::PrivateState::open(dir.path()).unwrap();
        storage::set_kind(&held.path, Kind::SystemKeyring).unwrap();
        let mut library = quince_core::Library::open(&held.path).unwrap();
        library.set_state("account", "synthetic-account").unwrap();
    }
    server
        .handle(json!({"id":0,"method":"initialize"}))
        .await
        .unwrap();
    // The scheduler's pass: the saved store holds no session, so the sign-in failed.
    assert_eq!(server.refresh().await, Err("login_required"));
    let reply = server
        .handle(
            json!({"id":1,"method":"tools/call","params":{"name":"auth_status","arguments":{}}}),
        )
        .await
        .unwrap();
    let status: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["status"], "login_required", "{status}");
    assert_eq!(status["session_checked"], true, "{status}");
}
