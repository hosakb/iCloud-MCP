//! Own test binary: the document writer is a process-wide first-wins value.
use icloud_mcp::Server;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

async fn call(server: &mut Server, name: &str, args: Value) -> Value {
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let response = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[tokio::test]
async fn server_starts_while_a_peer_holds_the_lock_and_reports_busy_per_call() {
    let dir = private_dir();
    let held = icloud_mcp::state::PrivateState::open(dir.path()).unwrap();
    let mut server = Server::open(dir.path()).expect("startup must not fail on a busy lock");
    let busy = call(&mut server, "notes_list", json!({})).await;
    assert_eq!(busy["error"], "state_busy_retry", "{busy}");
    drop(held);
    // The folder works again; without a sign-in a read says so.
    let read = call(&mut server, "notes_list", json!({})).await;
    assert_eq!(read["error"], "not_signed_in", "{read}");
    // The deferred setup still installs the persistent document writer.
    let library = quince_core::Library::open(dir.path()).unwrap();
    let id = library.state("documents.writer").unwrap().expect("writer");
    assert_eq!(quince_icloud::topotext::writer_id(), id.to_uppercase());
}
