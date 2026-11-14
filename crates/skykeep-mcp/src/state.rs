//! Linux private state ownership and cross-process serialization.
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub struct PrivateState {
    pub path: PathBuf,
    _lock: File,
}

impl PrivateState {
    pub fn open(path: &Path) -> Result<Self, &'static str> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|_| "state_unavailable")?
                .join(path)
        };
        let mut checked = PathBuf::new();
        for component in absolute.components() {
            if matches!(component, Component::ParentDir) {
                return Err("unsafe_state_path");
            }
            checked.push(component);
            if let Ok(meta) = std::fs::symlink_metadata(&checked) {
                if meta.file_type().is_symlink() {
                    return Err("unsafe_state_path");
                }
            }
        }
        if !absolute.exists() {
            // Parents must exist. Avoid creating an unvalidated chain of directories.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&absolute)
                .map_err(|_| "state_unavailable")?;
        }
        let meta = std::fs::symlink_metadata(&absolute).map_err(|_| "state_unavailable")?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err("state_must_be_owned_and_private");
        }
        // Reject links, foreign files, and shared files before SQLite or auth opens them.
        for entry in std::fs::read_dir(&absolute).map_err(|_| "state_unavailable")? {
            let entry = entry.map_err(|_| "state_unavailable")?;
            let m = std::fs::symlink_metadata(entry.path()).map_err(|_| "state_unavailable")?;
            if m.file_type().is_symlink() || m.uid() != meta.uid() || m.mode() & 0o077 != 0 {
                return Err("unsafe_state_contents");
            }
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(absolute.join("mcp.lock"))
            .map_err(|_| "state_unavailable")?;
        lock.try_lock().map_err(|_| "state_busy_retry")?;
        Ok(Self {
            path: absolute.canonicalize().map_err(|_| "state_unavailable")?,
            _lock: lock,
        })
    }
}
