use icloud_mcp::state::PrivateState;
use std::os::unix::fs::{symlink, PermissionsExt};

// Startup while another process holds the lock is covered in review_fixes.rs.

#[test]
fn state_is_private_and_exclusive() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp");
    let held = PrivateState::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(PrivateState::open(&path).is_err());
    drop(held);
    assert!(PrivateState::open(&path).is_ok());
}

#[test]
fn rejects_symlink_or_shared_state() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("actual");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = root.path().join("link");
    symlink(&dir, &link).unwrap();
    assert!(PrivateState::open(&link).is_err());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(PrivateState::open(&dir).is_err());
}
