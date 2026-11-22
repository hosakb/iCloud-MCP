//! The one local folder Drive uploads read from and downloads write into.
//!
//! The person sets it up in their own terminal (`skykeep-mcp exchange`); no MCP argument can
//! name or change it, or its size limit. Until it is set up, uploads and downloads are
//! refused. A tool argument names a file inside it by a relative path of plain names: an
//! absolute path, `..`, `.`, empty names, control and text-direction characters are refused.
//! Every name on the way is opened relative to the folder before it, without following a
//! symbolic link (`openat` with `O_NOFOLLOW`), so a link swapped in at any moment is refused
//! rather than followed. The folder itself is held to the device and inode it had when it was
//! set up. A download is written to a new hidden file beside its name and linked into place
//! only if the name is still free, so an existing file is never overwritten.
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

/// Where the folder and its limit are kept, in the private directory's state.
pub(crate) const KEY: &str = "mcp.drive.exchange";
/// The folder's name in the home folder, unless the person names another.
pub const DEFAULT_NAME: &str = "Skykeep-Exchange";
/// The size limit unless the person sets another: 100 MB.
pub const DEFAULT_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// The largest limit the terminal accepts, in MB.
pub const MAX_LIMIT_MB: u64 = 10_240;
/// Names in a local path argument.
pub(crate) const MAX_LOCAL_DEPTH: usize = 8;
/// Bytes of a local path argument.
pub(crate) const MAX_LOCAL_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 255;
/// How a partial download is named while it arrives.
const PARTIAL_PREFIX: &str = ".skykeep-download-";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    /// Absolute.
    pub path: PathBuf,
    /// Device and inode when it was set up.
    pub identity: (u64, u64),
    pub max_bytes: u64,
}

/// The home folder's `Skykeep-Exchange`.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(DEFAULT_NAME))
}

/// The folder as set up, `None` before it is.
pub fn configured(library: &quince_core::Library) -> Result<Option<Exchange>, &'static str> {
    match library.state(KEY).map_err(|_| "library_unavailable")? {
        None => Ok(None),
        Some(raw) if raw.is_empty() => Ok(None),
        Some(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|_| "invalid_exchange_setting"),
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn c_name(name: &str) -> Result<CString, &'static str> {
    CString::new(name).map_err(|_| "invalid_local_path")
}

fn fstat(fd: i32) -> Result<libc::stat, &'static str> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err("local_file_unavailable");
    }
    Ok(st)
}

/// Owned by this user and writable by nobody else.
fn private_enough(st: &libc::stat) -> bool {
    st.st_uid == unsafe { libc::geteuid() } && st.st_mode & 0o022 == 0
}

/// Opens a directory without following a link at its last name.
fn open_dir_at(parent: Option<&OwnedFd>, name: &CString) -> std::io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = match parent {
        Some(parent) => unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) },
        None => unsafe { libc::open(name.as_ptr(), flags) },
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Terminal only, under the private directory's lock: sets up the folder (`path`, else the one
/// already set up, else the default), creating it if it is missing, and the limit (`max_mb`,
/// else the one already set, else 100 MB). Nothing else is touched.
pub fn set_up(
    library: &mut quince_core::Library,
    path: Option<PathBuf>,
    max_mb: Option<u64>,
) -> Result<Exchange, &'static str> {
    let before = configured(library).ok().flatten();
    let path = match path {
        Some(path) => std::path::absolute(path).map_err(|_| "exchange_folder_unavailable")?,
        None => match &before {
            Some(before) => before.path.clone(),
            None => default_path().ok_or("home_folder_unknown_give_path")?,
        },
    };
    // Every name on the way: no `..` and no links, as for the private directory.
    let mut checked = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err("exchange_folder_path_unsafe");
        }
        checked.push(component);
        if std::fs::symlink_metadata(&checked).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("exchange_folder_path_unsafe");
        }
    }
    if std::fs::symlink_metadata(&path).is_err() {
        use std::os::unix::fs::DirBuilderExt;
        if !path.parent().is_some_and(Path::is_dir) {
            return Err("exchange_parent_missing");
        }
        // Only the folder itself: its parent must exist.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| "exchange_folder_unavailable")?;
    }
    let raw = c_name(path.to_str().ok_or("exchange_folder_path_unsafe")?)?;
    let fd = open_dir_at(None, &raw).map_err(|_| "exchange_folder_unavailable")?;
    let st = fstat(fd.as_raw_fd())?;
    if st.st_uid != unsafe { libc::geteuid() } {
        return Err("exchange_folder_not_yours");
    }
    if !private_enough(&st) {
        return Err("exchange_folder_writable_by_others");
    }
    let max_bytes = match max_mb {
        Some(mb) if (1..=MAX_LIMIT_MB).contains(&mb) => mb * 1024 * 1024,
        Some(_) => return Err("exchange_limit_out_of_range"),
        None => before.map_or(DEFAULT_MAX_BYTES, |b| b.max_bytes),
    };
    let exchange = Exchange {
        path,
        identity: (st.st_dev as u64, st.st_ino as u64),
        max_bytes,
    };
    let text = serde_json::to_string(&exchange).map_err(|_| "library_unavailable")?;
    library
        .set_state(KEY, &text)
        .map_err(|_| "library_unavailable")?;
    Ok(exchange)
}

/// What a person reads when `skykeep-mcp exchange` stops with `code`: what is wrong and what
/// to type. `path` is the folder it was setting up, `dir` the private folder.
pub fn explain(code: &str, path: Option<&Path>, dir: &Path) -> String {
    let state = crate::status::quoted(dir);
    let folder = path.map_or_else(|| "the folder".to_string(), crate::status::quoted);
    match code {
        "exchange_limit_out_of_range" => format!("--max-mb takes a whole number of megabytes from 1 to {MAX_LIMIT_MB}, for example: skykeep-mcp exchange --state-dir {state} --max-mb 100"),
        "exchange_parent_missing" => {
            let parent = path
                .and_then(Path::parent)
                .map_or_else(|| "its parent".to_string(), crate::status::quoted);
            format!("The folder that would hold {folder} does not exist. Create it with: mkdir -p {parent} and run the same command again.")
        }
        "exchange_folder_writable_by_others" => format!("Other users can change {folder}. Make it yours alone with: chmod 700 {folder} and run the same command again."),
        "exchange_folder_not_yours" => format!("{folder} belongs to another user. Pick a folder of your own with: skykeep-mcp exchange --state-dir {state} --path DIR"),
        "exchange_folder_path_unsafe" => format!("The path to {folder} has a symbolic link or .. in it. Give the real path, for example: skykeep-mcp exchange --state-dir {state} --path ~/{DEFAULT_NAME}"),
        "home_folder_unknown_give_path" => format!("Your home folder is not known here. Name the folder with: skykeep-mcp exchange --state-dir {state} --path DIR"),
        "exchange_folder_unavailable" => format!("{folder} could not be created or opened. Check that it is a folder you can open, or pick another with: skykeep-mcp exchange --state-dir {state} --path DIR"),
        "invalid_exchange_setting" => format!("The saved exchange setting is unreadable. Set it again with: skykeep-mcp exchange --state-dir {state} --path DIR"),
        "exchange_path_required" => "--path needs a folder after it, for example: --path ~/Skykeep-Exchange".to_string(),
        "state_busy_retry" => "Another Skykeep command is using the private folder. Run the same command again in a moment.".to_string(),
        other => format!("The exchange folder was not set up ({other}). Run skykeep-mcp status --state-dir {state} to check the private folder."),
    }
}

/// The first folder on the way to `local` inside the exchange folder that is not this user's
/// alone, for the instructions that say how to fix it. Looked at without following links.
pub(crate) fn shared_folder(state_dir: &Path, local: &str) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let library = quince_core::Library::open(state_dir).ok()?;
    let exchange = configured(&library).ok()??;
    let names = local_names(local).ok()?;
    let mut at = exchange.path;
    for name in names.split_last()?.1 {
        at.push(name);
        let meta = std::fs::symlink_metadata(&at).ok()?;
        if !meta.is_dir() {
            return None;
        }
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
            return Some(at);
        }
    }
    None
}

/// The folder, opened and checked to be the one set up.
fn open_root(exchange: &Exchange) -> Result<OwnedFd, &'static str> {
    let raw = c_name(
        exchange
            .path
            .to_str()
            .ok_or("exchange_folder_changed_run_skykeep_mcp_exchange")?,
    )?;
    let fd =
        open_dir_at(None, &raw).map_err(|_| "exchange_folder_missing_run_skykeep_mcp_exchange")?;
    let st = fstat(fd.as_raw_fd())?;
    if (st.st_dev as u64, st.st_ino as u64) != exchange.identity || !private_enough(&st) {
        return Err("exchange_folder_changed_run_skykeep_mcp_exchange");
    }
    Ok(fd)
}

/// Whether `c` reorders or hides text when shown: direction marks, embeddings, overrides and
/// isolates, and the zero-width characters.
pub(crate) fn hides_text(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// The names of a local path argument, relative to the folder.
pub(crate) fn local_names(arg: &str) -> Result<Vec<String>, &'static str> {
    if arg.len() > MAX_LOCAL_BYTES {
        return Err("local_path_too_long");
    }
    if arg.starts_with('/') || arg.starts_with('~') {
        return Err("absolute_local_path_refused");
    }
    let names: Vec<&str> = arg.split('/').collect();
    if names.len() > MAX_LOCAL_DEPTH {
        return Err("local_path_too_deep");
    }
    for name in &names {
        if *name == "." || *name == ".." {
            return Err("local_path_traversal_refused");
        }
        if name.is_empty()
            || name.len() > MAX_NAME_BYTES
            || name.chars().any(|c| c.is_control() || hides_text(c))
        {
            return Err("invalid_local_path");
        }
    }
    Ok(names.into_iter().map(ToString::to_string).collect())
}

/// A name from Apple made safe to show and to use as one local name: control and hiding
/// characters removed. `None` when what is left could leave the folder or is no name.
pub(crate) fn local_name_from_apple(name: &str) -> Option<String> {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !hides_text(*c))
        .collect();
    let usable = !clean.is_empty()
        && clean != "."
        && clean != ".."
        && !clean.contains('/')
        && !clean.starts_with(PARTIAL_PREFIX)
        && clean.len() <= MAX_NAME_BYTES;
    usable.then_some(clean)
}

/// The folder that holds the last name, every name on the way opened without following links.
fn walk(root: OwnedFd, names: &[String]) -> Result<OwnedFd, &'static str> {
    let mut at = root;
    for name in names {
        let raw = c_name(name)?;
        at = open_dir_at(Some(&at), &raw).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => "local_symlink_refused",
            Some(libc::ENOENT) => "local_folder_not_found",
            Some(libc::ENOTDIR) => {
                if is_link(&at, &raw) {
                    "local_symlink_refused"
                } else {
                    "local_not_a_folder"
                }
            }
            _ => "local_file_unavailable",
        })?;
        // A folder another user can change could swap what is in it under us.
        if !private_enough(&fstat(at.as_raw_fd())?) {
            return Err("local_folder_not_private");
        }
    }
    Ok(at)
}

fn is_link(dir: &OwnedFd, name: &CString) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let found = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    found == 0 && st.st_mode & libc::S_IFMT == libc::S_IFLNK
}

/// A file to upload, opened for reading: a regular file with one name, at most the limit.
pub(crate) fn open_upload(exchange: &Exchange, local: &str) -> Result<(File, u64), &'static str> {
    let names = local_names(local)?;
    let (last, dirs) = names.split_last().ok_or("invalid_local_path")?;
    let dir = walk(open_root(exchange)?, dirs)?;
    let raw = c_name(last)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            raw.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
        )
    };
    if fd < 0 {
        return Err(match errno() {
            libc::ELOOP => "local_symlink_refused",
            libc::ENOENT => "local_file_not_found",
            _ => "local_file_unavailable",
        });
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let st = fstat(file.as_raw_fd())?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err("local_not_a_regular_file");
    }
    // A second name may be anywhere, outside the folder too.
    if st.st_nlink != 1 {
        return Err("local_hard_link_refused");
    }
    let size = u64::try_from(st.st_size).map_err(|_| "local_file_unavailable")?;
    if size > exchange.max_bytes {
        return Err("local_file_over_exchange_limit");
    }
    Ok((file, size))
}

/// A download on its way in: a new hidden file in the target folder, linked to its name only
/// once it is whole and only if the name is still free. Removed if it never is.
pub(crate) struct Incoming {
    dir: OwnedFd,
    partial: CString,
    name: CString,
    pub(crate) file: Option<File>,
    placed: bool,
}

impl Incoming {
    /// The file is whole: synced and given its name, or refused if the name was taken.
    pub(crate) fn place(mut self) -> Result<(), &'static str> {
        let file = self.file.take().ok_or("local_file_unavailable")?;
        file.sync_all().map_err(|_| "local_write_failed")?;
        drop(file);
        let linked = unsafe {
            libc::linkat(
                self.dir.as_raw_fd(),
                self.partial.as_ptr(),
                self.dir.as_raw_fd(),
                self.name.as_ptr(),
                0,
            )
        };
        if linked != 0 {
            let error = errno();
            if error == libc::EEXIST {
                return Err("local_file_exists_not_overwritten");
            }
            // A filesystem without hard links: a rename that refuses an existing name.
            let renamed = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    self.dir.as_raw_fd(),
                    self.partial.as_ptr(),
                    self.dir.as_raw_fd(),
                    self.name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if renamed != 0 {
                return Err(if errno() == libc::EEXIST {
                    "local_file_exists_not_overwritten"
                } else {
                    "local_write_failed"
                });
            }
            self.placed = true;
            return Ok(());
        }
        self.placed = true;
        unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.partial.as_ptr(), 0) };
        Ok(())
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.placed {
            unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.partial.as_ptr(), 0) };
        }
    }
}

/// A partial download untouched this long was cut off (a crash or a power cut): every chunk
/// written moves its time on.
const STALE: std::time::Duration = std::time::Duration::from_secs(600);

/// Removes stale partial downloads from `dir`: regular files of this user named as partial
/// downloads are, not changed for [`STALE`]. Best effort.
fn sweep(dir: &OwnedFd) {
    let copy = unsafe { libc::dup(dir.as_raw_fd()) };
    if copy < 0 {
        return;
    }
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        unsafe { libc::close(copy) };
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut stale = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let text = name.to_string_lossy();
        if text.starts_with(PARTIAL_PREFIX) && text.ends_with(".part") {
            stale.push(name.to_owned());
        }
    }
    unsafe { libc::closedir(stream) };
    for name in stale {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let found = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if found == 0
            && st.st_mode & libc::S_IFMT == libc::S_IFREG
            && st.st_uid == unsafe { libc::geteuid() }
            && now.saturating_sub(st.st_mtime as i64) >= STALE.as_secs() as i64
        {
            unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
        }
    }
}

/// Starts a download into `names` (relative to the folder). Refused when the name is taken.
pub(crate) fn incoming(exchange: &Exchange, names: &[String]) -> Result<Incoming, &'static str> {
    let (last, dirs) = names.split_last().ok_or("invalid_local_path")?;
    let dir = walk(open_root(exchange)?, dirs)?;
    sweep(&dir);
    let name = c_name(last)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Err("local_file_exists_not_overwritten");
    }
    let partial = c_name(&format!("{PARTIAL_PREFIX}{}.part", uuid::Uuid::new_v4()))?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            partial.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err("local_write_failed");
    }
    Ok(Incoming {
        dir,
        partial,
        name,
        file: Some(unsafe { File::from_raw_fd(fd) }),
        placed: false,
    })
}

/// A private copy of an upload in the private directory, named as the file will be in iCloud
/// Drive: read once, under the limit as it is read, so what is sent is exactly what was read.
/// Removed when dropped, and anything an earlier interrupted upload left is removed first.
pub(crate) struct Staged {
    folder: PathBuf,
    pub(crate) path: PathBuf,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

/// Where uploads are staged, inside the private directory.
const STAGING: &str = "drive-upload";

/// Caller holds the private directory's lock.
pub(crate) fn stage(
    state_dir: &Path,
    mut source: File,
    expected: u64,
    max_bytes: u64,
    name: &str,
) -> Result<Staged, &'static str> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let base = state_dir.join(STAGING);
    match std::fs::symlink_metadata(&base) {
        Ok(m) if m.is_dir() => {
            // What an interrupted upload left behind.
            for entry in std::fs::read_dir(&base).map_err(|_| "staging_unavailable")? {
                let entry = entry.map_err(|_| "staging_unavailable")?;
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(_) => return Err("staging_unavailable"),
        Err(_) => std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .map_err(|_| "staging_unavailable")?,
    }
    let folder = base.join(uuid::Uuid::new_v4().to_string());
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&folder)
        .map_err(|_| "staging_unavailable")?;
    let staged = Staged {
        path: folder.join(name),
        folder,
    };
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staged.path)
        .map_err(|_| "staging_unavailable")?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("local_file_unavailable"),
        };
        total += n as u64;
        if total > max_bytes {
            return Err("local_file_over_exchange_limit");
        }
        out.write_all(&buf[..n])
            .map_err(|_| "staging_unavailable")?;
    }
    if total != expected {
        return Err("local_file_changed_while_read");
    }
    out.sync_all().map_err(|_| "staging_unavailable")?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths_are_plain_relative_names() {
        assert_eq!(local_names("a.txt"), Ok(vec!["a.txt".to_string()]));
        assert_eq!(
            local_names("in/a b.txt"),
            Ok(vec!["in".to_string(), "a b.txt".to_string()])
        );
        for (bad, code) in [
            ("/etc/passwd", "absolute_local_path_refused"),
            ("~/x", "absolute_local_path_refused"),
            ("../x", "local_path_traversal_refused"),
            ("a/../../x", "local_path_traversal_refused"),
            ("./x", "local_path_traversal_refused"),
            ("", "invalid_local_path"),
            ("a//b", "invalid_local_path"),
            ("a/", "invalid_local_path"),
            ("a\u{0}b", "invalid_local_path"),
            ("a\u{202E}txt.exe", "invalid_local_path"),
        ] {
            assert_eq!(local_names(bad), Err(code), "{bad:?}");
        }
        assert_eq!(
            local_names("a/".repeat(9).trim_end_matches('/')),
            Err("local_path_too_deep")
        );
    }

    #[test]
    fn apple_names_lose_hiding_characters_and_never_leave_the_folder() {
        assert_eq!(
            local_name_from_apple("in\u{202E}voice\u{7}.pdf").as_deref(),
            Some("invoice.pdf")
        );
        for bad in [
            "..",
            ".",
            "a/b",
            "",
            "\u{200B}",
            "\u{7}..",
            ".skykeep-download-x.part",
        ] {
            assert_eq!(local_name_from_apple(bad), None, "{bad:?}");
        }
    }
}
