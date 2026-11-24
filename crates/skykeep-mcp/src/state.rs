//! Linux private state ownership and cross-process serialization.
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub struct PrivateState {
    pub path: PathBuf,
    _lock: File,
}

/// How many times this process asked for a folder's lock.
static OPENS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times this process asked for a folder's lock, so a test can show that a call
/// and what it records share one hold.
#[doc(hidden)]
pub fn opens() -> usize {
    OPENS.load(std::sync::atomic::Ordering::SeqCst)
}

impl PrivateState {
    pub fn open(path: &Path) -> Result<Self, &'static str> {
        OPENS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            // The permissions file holds no secret. Editors and agents often save it shared
            // or as a link; its own reader refuses it then, and the folder stays usable.
            if entry.file_name() == crate::permissions::FILE_NAME {
                continue;
            }
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

/// The first entry that makes [`PrivateState::open`] refuse a folder with
/// `unsafe_state_contents`, so a message can name it, and whether it is a link.
pub fn unsafe_entry(dir: &Path) -> Option<(PathBuf, bool)> {
    let meta = std::fs::symlink_metadata(dir).ok()?;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if entry.file_name() == crate::permissions::FILE_NAME {
            continue;
        }
        let m = std::fs::symlink_metadata(entry.path()).ok()?;
        let link = m.file_type().is_symlink();
        if link || m.uid() != meta.uid() || m.mode() & 0o077 != 0 {
            return Some((entry.path(), link));
        }
    }
    None
}

/// One sentence on the entry that keeps Skykeep from using the folder, and how to fix it.
pub fn unsafe_entry_sentence(dir: &Path) -> Option<String> {
    let (path, link) = unsafe_entry(dir)?;
    let shown = crate::status::quoted(&path);
    Some(if link {
        format!(
            "iCloud-MCP won't use its folder because {shown} is a link. Remove the link: rm {shown}"
        )
    } else {
        format!("iCloud-MCP won't use its folder because others can read or change {shown}. Delete it if it is an editor's backup copy, or run: chmod 600 {shown}")
    })
}
