//! The one local folder Drive and Photos uploads read from and Drive downloads write into.
//!
//! The person names it in `permissions.yaml` (`exchange_folder:`) or sets it up in their own
//! terminal (`skykeep-mcp exchange`, which writes that line); the file's value wins. No MCP
//! argument can name or change it, or its size limit, which only the terminal sets. Until it
//! is set up, uploads and downloads are refused. Before every transfer the folder is checked
//! ([`checked`]): an existing private folder of this user, not a link, not a system folder or
//! the home folder, not inside a hidden, credentials or keyring folder, and apart from the
//! private state folder. A folder that fails is refused; nothing falls back to another one. A tool argument names a file inside it by a relative path of plain names: an
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
    /// Named in permissions.yaml: a later failure points to the file, not the command.
    #[serde(skip)]
    pub from_file: bool,
}

/// The home folder's `Skykeep-Exchange`.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(DEFAULT_NAME))
}

/// Folders that belong to the system. One of them, or anything inside one, is never an
/// exchange folder, unless it is inside the home folder (as `/var/home/NAME` can be).
const SYSTEM: [&str; 17] = [
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/opt", "/proc",
    "/root", "/run", "/sbin", "/snap", "/srv", "/sys", "/usr",
];
/// Shared folders that are never an exchange folder themselves.
const SHARED: [&str; 6] = ["/home", "/media", "/mnt", "/tmp", "/var", "/var/tmp"];

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// The rules on where the folder is, applied to its real path.
fn place_rules(real: &Path, state_dir: &Path) -> Result<(), &'static str> {
    let home = home().and_then(|h| std::fs::canonicalize(h).ok());
    let in_home = home
        .as_ref()
        .is_some_and(|h| real.starts_with(h) && real != h);
    // The root, the home folder and every folder above it hold far more than an exchange.
    if real.parent().is_none()
        || home.as_ref().is_some_and(|h| h.starts_with(real))
        || SHARED.iter().any(|s| real == Path::new(s))
        || (!in_home && SYSTEM.iter().any(|s| real.starts_with(s)))
        || (!in_home && real.starts_with("/var") && !real.starts_with("/var/home"))
    {
        return Err("exchange_folder_system_folder");
    }
    let state = std::fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf());
    if real.starts_with(&state) || state.starts_with(real) {
        return Err("exchange_folder_private_state");
    }
    // Inside the home folder, only the names below it count: the person picks those.
    let below = match &home {
        Some(h) if in_home => real.strip_prefix(h).unwrap_or(real),
        _ => real,
    };
    let hidden = below.components().any(|c| match c {
        Component::Normal(name) => {
            let name = name.to_string_lossy().to_lowercase();
            name.starts_with('.') || name.contains("credential") || name.contains("keyring")
        }
        _ => false,
    });
    if hidden {
        return Err("exchange_folder_hidden_or_credentials");
    }
    Ok(())
}

/// The path `written` names, with a leading `~` as the home folder.
pub fn expand(written: &str) -> Result<PathBuf, &'static str> {
    Ok(match written.strip_prefix('~') {
        Some("") => home().ok_or("exchange_folder_home_unknown")?,
        Some(rest) if rest.starts_with('/') => home()
            .ok_or("exchange_folder_home_unknown")?
            .join(rest.trim_start_matches('/')),
        _ => PathBuf::from(written),
    })
}

/// The folder `written` names (a full path or `~/...`), checked as it is now: see the module
/// notes. `state_dir` is the private state folder; `max_bytes` the size limit to use.
pub fn checked(written: &str, state_dir: &Path, max_bytes: u64) -> Result<Exchange, &'static str> {
    let path = expand(written)?;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("exchange_folder_not_a_full_path");
    }
    let meta = std::fs::symlink_metadata(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "exchange_folder_missing",
        _ => "exchange_folder_unavailable",
    })?;
    if meta.file_type().is_symlink() {
        return Err("exchange_folder_is_a_link");
    }
    if !meta.is_dir() {
        return Err("exchange_folder_not_a_folder");
    }
    let raw = CString::new(path.to_str().ok_or("exchange_folder_unavailable")?)
        .map_err(|_| "exchange_folder_unavailable")?;
    let fd = open_dir_at(None, &raw).map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP) => "exchange_folder_is_a_link",
        Some(libc::ENOENT) => "exchange_folder_missing",
        Some(libc::ENOTDIR) => "exchange_folder_not_a_folder",
        _ => "exchange_folder_unavailable",
    })?;
    // Where the open folder really is, links above it followed, as the kernel sees it.
    let real = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .map_err(|_| "exchange_folder_unavailable")?;
    place_rules(&real, state_dir)?;
    let st = fstat(fd.as_raw_fd()).map_err(|_| "exchange_folder_unavailable")?;
    if st.st_uid != unsafe { libc::geteuid() } {
        return Err("exchange_folder_not_yours");
    }
    if !private_enough(&st) {
        return Err("exchange_folder_writable_by_others");
    }
    Ok(Exchange {
        path: real,
        identity: (st.st_dev as u64, st.st_ino as u64),
        max_bytes,
        from_file: false,
    })
}

/// The folder uploads and downloads use now, checked: the one `permissions.yaml` names (or
/// its last valid one while the file can't be used), else the one `skykeep-mcp exchange` set
/// up, else `None`. A folder that fails the checks is refused, never replaced by another.
pub fn in_force(
    library: &quince_core::Library,
    state_dir: &Path,
) -> Result<Option<Exchange>, &'static str> {
    let active = crate::permissions::peek(library, state_dir, None)?;
    let stored = configured(library);
    let max_bytes = stored
        .as_ref()
        .ok()
        .and_then(|s| s.as_ref())
        .map_or(DEFAULT_MAX_BYTES, |s| s.max_bytes);
    if let Some(written) = &active.exchange {
        let mut exchange = checked(written, state_dir, max_bytes)?;
        exchange.from_file = true;
        return Ok(Some(exchange));
    }
    let Some(stored) = stored? else {
        return Ok(None);
    };
    let text = stored
        .path
        .to_str()
        .ok_or("exchange_folder_changed_run_skykeep_mcp_exchange")?;
    let now = checked(text, state_dir, stored.max_bytes).map_err(|e| match e {
        "exchange_folder_missing" => "exchange_folder_missing_run_skykeep_mcp_exchange",
        other => other,
    })?;
    if now.identity != stored.identity || now.path != stored.path {
        return Err("exchange_folder_changed_run_skykeep_mcp_exchange");
    }
    Ok(Some(stored))
}

/// What is wrong with the folder, after its name, for `code` from [`checked`].
pub fn problem(code: &str) -> &'static str {
    match code {
        "exchange_folder_missing" | "exchange_folder_missing_run_skykeep_mcp_exchange" => {
            "does not exist"
        }
        "exchange_folder_not_a_folder" => "is not a folder",
        "exchange_folder_is_a_link" => "is a symbolic link",
        "exchange_folder_not_yours" => "belongs to another user",
        "exchange_folder_writable_by_others" => "can be changed by other users",
        "exchange_folder_system_folder" => {
            "is a system or shared folder, your home folder or a folder above it"
        }
        "exchange_folder_hidden_or_credentials" => "is inside a hidden folder (a name that starts with a dot, such as .ssh or .gnupg) or a credentials or keyring folder",
        "exchange_folder_private_state" => "is the private folder (--state-dir), is inside it or holds it",
        "exchange_folder_home_unknown" => "starts with ~, but the home folder is not known here",
        "exchange_folder_not_a_full_path" => "is not a full path",
        "exchange_folder_changed_run_skykeep_mcp_exchange" => {
            "is no longer the folder that was set up"
        }
        "exchange_folder_changed" => "changed while it was in use; try again",
        _ => "can't be opened",
    }
}

/// The next step for `code`. `written` is the folder the file names, when it names one.
pub fn fix(code: &str, dir: &Path, written: Option<&str>) -> String {
    let file = crate::status::quoted(&dir.join(crate::permissions::FILE_NAME));
    let terminal = crate::status::with_dir("skykeep-mcp exchange --path DIR", dir);
    let Some(written) = written else {
        return format!(
            "In your own terminal, run: {terminal} with a private folder of your own, or name one with {}: in {file}.",
            crate::permissions::EXCHANGE_KEY
        );
    };
    // Expanded first: a quoted ~ would not be expanded by the shell.
    let folder = crate::status::quoted(&expand(written).unwrap_or_else(|_| PathBuf::from(written)));
    let change = format!(
        "change {}: in {file} to a private folder of your own, such as ~/{DEFAULT_NAME}, or run {terminal} in your own terminal",
        crate::permissions::EXCHANGE_KEY
    );
    match code {
        "exchange_folder_missing" => {
            format!("Create it in your own terminal with: mkdir -m 700 {folder} or {change}.")
        }
        "exchange_folder_writable_by_others" => {
            format!("Make it yours alone with: chmod 700 {folder} or {change}.")
        }
        _ => {
            let mut text = change;
            text[..1].make_ascii_uppercase();
            format!("{text}.")
        }
    }
}

/// The instructions a tool result carries for an exchange folder error.
pub fn next_step(code: &str, dir: &Path) -> String {
    // Read only: a folder without a library gets none made here.
    let written = if dir.join("library.db").is_file() {
        quince_core::Library::open(dir)
            .ok()
            .and_then(|l| crate::permissions::peek(&l, dir, None).ok())
            .and_then(|a| a.exchange)
    } else {
        crate::permissions::setting_in_file(dir, crate::permissions::EXCHANGE_KEY)
    };
    if code == "exchange_folder_not_set_up_run_skykeep_mcp_exchange_in_a_terminal" {
        return fix(code, dir, None);
    }
    match &written {
        Some(w) => format!(
            "The exchange folder {} in {} {}, so nothing was uploaded or downloaded. {}",
            crate::status::quoted(&expand(w).unwrap_or_else(|_| PathBuf::from(w))),
            crate::permissions::FILE_NAME,
            problem(code),
            fix(code, dir, Some(w))
        ),
        None => format!(
            "The exchange folder {}, so nothing was uploaded or downloaded. {}",
            problem(code),
            fix(code, dir, None)
        ),
    }
}

/// The folder as `skykeep-mcp exchange` stored it, `None` before it is.
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
    set_up_in(library, None, path, max_mb)
}

/// [`set_up`] for the terminal command: the folder must also pass [`checked`] for the private
/// folder `state_dir`, before anything is saved.
pub fn set_up_checked(
    library: &mut quince_core::Library,
    state_dir: &Path,
    path: Option<PathBuf>,
    max_mb: Option<u64>,
) -> Result<Exchange, &'static str> {
    set_up_in(library, Some(state_dir), path, max_mb)
}

fn set_up_in(
    library: &mut quince_core::Library,
    state_dir: Option<&Path>,
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
    let path = real_path(&path)?;
    if let Some(state_dir) = state_dir {
        // Where the folder would be is checked before it is created.
        place_rules(&path, state_dir)?;
    }
    if std::fs::symlink_metadata(&path).is_err() {
        use std::os::unix::fs::DirBuilderExt;
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
    if let Some(state_dir) = state_dir {
        let text = path.to_str().ok_or("exchange_folder_path_unsafe")?;
        checked(text, state_dir, max_bytes)?;
    }
    let exchange = Exchange {
        path,
        identity: (st.st_dev as u64, st.st_ino as u64),
        max_bytes,
        from_file: false,
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
        "exchange_folder_path_unsafe" => match path.and_then(|p| std::fs::canonicalize(p).ok()) {
            Some(real) => format!("{folder} is a symbolic link. Use the real folder it points to: skykeep-mcp exchange --state-dir {state} --path {}", crate::status::quoted(&real)),
            None => format!("{folder} is a symbolic link or names no folder. Give a real folder, for example: skykeep-mcp exchange --state-dir {state} --path ~/{DEFAULT_NAME}"),
        },
        "home_folder_unknown_give_path" => format!("Your home folder is not known here. Name the folder with: skykeep-mcp exchange --state-dir {state} --path DIR"),
        "exchange_folder_unavailable" => format!("{folder} could not be created or opened. Check that it is a folder you can open, or pick another with: skykeep-mcp exchange --state-dir {state} --path DIR"),
        "invalid_exchange_setting" => format!("The saved exchange setting is unreadable. Set it again with: skykeep-mcp exchange --state-dir {state} --path DIR"),
        "exchange_path_required" => "--path needs a folder after it, for example: --path ~/Skykeep-Exchange".to_string(),
        "exchange_folder_system_folder" | "exchange_folder_hidden_or_credentials" | "exchange_folder_private_state" | "exchange_folder_is_a_link" | "exchange_folder_not_a_folder" | "exchange_folder_home_unknown" => format!("{folder} {}. Pick a private folder of your own, for example: skykeep-mcp exchange --state-dir {state} --path ~/{DEFAULT_NAME}", problem(code)),
        "permissions_file_unusable" => format!("permissions.yaml in {state} can't be edited, so nothing was changed. Run skykeep-mcp permissions show --state-dir {state} to see what to fix, then run the same command again."),
        "state_busy_retry" => "Another Skykeep command is using the private folder. Run the same command again in a moment.".to_string(),
        other => format!("The exchange folder was not set up ({other}). Run skykeep-mcp status --state-dir {state} to check the private folder."),
    }
}

/// The first folder on the way to `local` inside the exchange folder that is not this user's
/// alone, for the instructions that say how to fix it. Looked at without following links.
pub(crate) fn shared_folder(state_dir: &Path, local: &str) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let library = quince_core::Library::open(state_dir).ok()?;
    let exchange = in_force(&library, state_dir).ok()??;
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

/// `path` with the folders above it resolved, once, here in the person's own terminal: a link
/// among them (such as `/home` pointing to `/var/home`) is followed and the real path kept.
/// The exchange folder itself must not be a link, and every name inside it is opened without
/// following one at transfer time.
fn real_path(path: &Path) -> Result<PathBuf, &'static str> {
    let name = path.file_name().ok_or("exchange_folder_path_unsafe")?;
    if path
        .components()
        .next_back()
        .is_some_and(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("exchange_folder_path_unsafe");
    }
    let parent = path.parent().ok_or("exchange_folder_path_unsafe")?;
    let parent = std::fs::canonicalize(parent).map_err(|_| "exchange_parent_missing")?;
    if !parent.is_dir() {
        return Err("exchange_parent_missing");
    }
    let path = parent.join(name);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("exchange_folder_path_unsafe");
    }
    Ok(path)
}

/// The folder, opened and checked to be the one set up.
fn open_root(exchange: &Exchange) -> Result<OwnedFd, &'static str> {
    // A folder named in permissions.yaml gets codes whose next step points to the file.
    let (missing, changed) = if exchange.from_file {
        ("exchange_folder_missing", "exchange_folder_changed")
    } else {
        (
            "exchange_folder_missing_run_skykeep_mcp_exchange",
            "exchange_folder_changed_run_skykeep_mcp_exchange",
        )
    };
    let raw = c_name(exchange.path.to_str().ok_or(changed)?)?;
    let fd = open_dir_at(None, &raw).map_err(|_| missing)?;
    let st = fstat(fd.as_raw_fd())?;
    if (st.st_dev as u64, st.st_ino as u64) != exchange.identity || !private_enough(&st) {
        return Err(changed);
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
    fn a_folder_from_the_file_that_fails_at_use_points_to_the_file() {
        let gone = |from_file| Exchange {
            path: PathBuf::from("/nonexistent-exchange-for-test/in"),
            identity: (0, 0),
            max_bytes: 1,
            from_file,
        };
        assert_eq!(
            open_root(&gone(true)).map(|_| ()),
            Err("exchange_folder_missing")
        );
        assert_eq!(
            open_root(&gone(false)).map(|_| ()),
            Err("exchange_folder_missing_run_skykeep_mcp_exchange")
        );
        let here = std::env::current_dir().unwrap();
        let moved = Exchange {
            path: here,
            identity: (0, 0),
            max_bytes: 1,
            from_file: true,
        };
        assert_eq!(
            open_root(&moved).map(|_| ()),
            Err("exchange_folder_changed")
        );
    }

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
