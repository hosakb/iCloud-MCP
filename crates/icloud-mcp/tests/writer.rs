use icloud_mcp::Server;
use std::os::unix::fs::PermissionsExt;
#[test]
fn document_writer_is_installed_from_persistent_library_identity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::open(dir.path()).unwrap();
    let library = quince_core::Library::open(dir.path()).unwrap();
    let id = library
        .state("documents.writer")
        .unwrap()
        .expect("persistent writer");
    assert_eq!(quince_icloud::topotext::writer_id(), id.to_uppercase());
    drop(server);
    let _server = Server::open(dir.path()).unwrap();
    assert_eq!(library.state("documents.writer").unwrap().unwrap(), id);
}
