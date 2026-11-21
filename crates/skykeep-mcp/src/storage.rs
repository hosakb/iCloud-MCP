//! Where a folder's sign-in is kept: the system keyring, or a passphrase-protected file.
//!
//! The choice is written to `sign-in-storage` in the folder. A folder without that file keeps
//! its sign-in in the system keyring, as every folder did before the protected file existed.
//! The protected file (`sign-in.protected`) is sealed with a key derived from the person's
//! passphrase (see [`quince_auth::vault`]); `skykeep-mcp unlock` hands that key to the kernel
//! key storage ([`crate::keyhold`]) so the server can open the file. Nothing here ever writes a
//! secret in the clear, and nothing falls back to a weaker store.
use crate::keyhold::{HoldError, KernelKeyring, KeyHolder};
use quince_auth::vault::{self, Params, Unopened};
use quince_auth::{PlatformSecrets, Store};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

/// What every tool says while the protected file waits for its passphrase.
pub const LOCKED: &str = "locked: run skykeep-mcp unlock in a terminal";
/// What every tool says when the chosen storage was never finished.
pub const NOT_SET_UP: &str = "sign-in storage not ready: run skykeep-mcp setup in a terminal";
const MARKER: &str = "sign-in-storage";
pub const PROTECTED_FILE: &str = "sign-in.protected";
/// The shortest passphrase setup accepts.
pub const MIN_PASSPHRASE: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    SystemKeyring,
    ProtectedFile,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::SystemKeyring => "system-keyring",
            Kind::ProtectedFile => "protected-file",
        }
    }

    /// How the kind is named to the person.
    pub fn describe(self) -> &'static str {
        match self {
            Kind::SystemKeyring => "system keyring",
            Kind::ProtectedFile => "protected file",
        }
    }
}

/// The kind chosen for `dir`, or none when nothing was chosen yet.
pub fn kind(dir: &Path) -> Result<Option<Kind>, &'static str> {
    match std::fs::read_to_string(dir.join(MARKER)) {
        Ok(text) => match text.trim() {
            "system-keyring" => Ok(Some(Kind::SystemKeyring)),
            "protected-file" => Ok(Some(Kind::ProtectedFile)),
            _ => Err("sign-in storage setting is damaged: run skykeep-mcp setup in a terminal"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("state_unavailable"),
    }
}

/// The kind `dir` uses: the chosen one, or the system keyring for a folder that never chose.
pub fn effective_kind(dir: &Path) -> Result<Kind, &'static str> {
    Ok(kind(dir)?.unwrap_or(Kind::SystemKeyring))
}

/// Caller holds the folder's lock.
pub fn set_kind(dir: &Path, kind: Kind) -> Result<(), &'static str> {
    write_private(&dir.join(MARKER), format!("{}\n", kind.word()).as_bytes())
        .map_err(|_| "state_unavailable")
}

/// Written beside, made sure of on disk, then renamed over: whole or not at all, mode 0600.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let part = path.with_file_name(format!(
        ".{}.part",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&part)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&part, path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    written
}

/// Opens a folder's system keyring store.
pub type KeyringOpener = Box<dyn Fn(&Path) -> std::io::Result<Store> + Send + Sync>;

/// The system keyring store and the key holder a process uses.
pub struct Env {
    pub keyring: KeyringOpener,
    pub holder: Box<dyn KeyHolder>,
}

impl Env {
    /// Production: the folder's own Secret Service entry and the kernel key storage.
    pub fn system() -> Self {
        Self {
            keyring: Box::new(Store::isolated_keyring),
            holder: Box::new(KernelKeyring),
        }
    }
}

static ENV: std::sync::OnceLock<Env> = std::sync::OnceLock::new();

/// Replaces [`Env::system`] for this process, once, before any store is opened. Tests only.
pub fn use_env(env: Env) -> Result<(), &'static str> {
    ENV.set(env).map_err(|_| "storage_already_chosen")
}

pub fn env() -> &'static Env {
    ENV.get_or_init(Env::system)
}

/// The kernel key name for `dir`: tied to the folder itself (device and inode), like the
/// approval policy, so a copied folder never finds the original's key.
pub fn key_name(dir: &Path) -> Result<String, &'static str> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir).map_err(|_| "state_unavailable")?;
    Ok(format!("skykeep-mcp:{}:{}", meta.dev(), meta.ino()))
}

/// The names and values the file holds, cleared from memory when dropped.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct Entries(BTreeMap<String, String>);

impl Drop for Entries {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

/// An open protected file: the session store [`Store::with_secrets`] reads and writes.
pub struct ProtectedFile {
    path: PathBuf,
    params: Params,
    key: Zeroizing<vault::Key>,
}

/// Why a protected file did not open.
#[derive(Debug, PartialEq, Eq)]
pub enum Closed {
    /// No file yet.
    Missing,
    /// No key held, or the held key does not open it.
    Locked,
    /// The passphrase is wrong, or the file is damaged. Deliberately one answer.
    WrongOrDamaged,
    /// Written by a newer build.
    Newer,
    /// The kernel key storage is blocked here.
    HolderUnavailable,
    Failed,
}

impl Closed {
    /// One sentence for the person; never anything from the file.
    pub fn sentence(&self) -> &'static str {
        match self {
            Closed::Missing => "No protected file yet: run skykeep-mcp setup in a terminal.",
            Closed::Locked => "The protected file is locked: run skykeep-mcp unlock in a terminal.",
            Closed::WrongOrDamaged => "Wrong passphrase or damaged file.",
            Closed::Newer => "The protected file was written by a newer skykeep-mcp: update skykeep-mcp.",
            Closed::HolderUnavailable => "This computer blocks the kernel key storage that the protected file needs, so it can't be unlocked here. If you are inside Docker or a similar container, run Skykeep directly on the computer.",
            Closed::Failed => "The protected file could not be read or written.",
        }
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, Closed> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Closed::Missing),
        Err(_) => return Err(Closed::Failed),
    };
    let mut bytes = Vec::new();
    file.take(16 << 20)
        .read_to_end(&mut bytes)
        .map_err(|_| Closed::Failed)?;
    Ok(bytes)
}

impl ProtectedFile {
    fn opened(&self) -> Result<Entries, String> {
        let bytes = read_file(&self.path).map_err(|c| c.sentence().to_string())?;
        let plain = Zeroizing::new(
            vault::open(&self.key, &bytes)
                .map_err(|_| Closed::WrongOrDamaged.sentence().to_string())?,
        );
        serde_json::from_slice(&plain).map_err(|_| Closed::WrongOrDamaged.sentence().to_string())
    }

    fn write(&self, entries: &Entries) -> Result<(), String> {
        let plain = Zeroizing::new(serde_json::to_vec(entries).map_err(|e| e.to_string())?);
        let sealed = vault::seal(&self.key, &self.params, &plain).map_err(|e| e.to_string())?;
        write_private(&self.path, &sealed).map_err(|_| Closed::Failed.sentence().to_string())
    }

    /// The file under `dir`, opened with the key the holder keeps for it.
    pub fn unlocked(dir: &Path, holder: &dyn KeyHolder) -> Result<Self, Closed> {
        let path = dir.join(PROTECTED_FILE);
        let bytes = read_file(&path)?;
        let params = match Params::of(&bytes) {
            Ok(p) => p,
            Err(Unopened::Version(_)) => return Err(Closed::Newer),
            Err(Unopened::Damaged) => return Err(Closed::WrongOrDamaged),
        };
        let name = key_name(dir).map_err(|_| Closed::Failed)?;
        let held = match holder.get(&name) {
            Ok(Some(k)) => k,
            Ok(None) => return Err(Closed::Locked),
            Err(HoldError::Unavailable) => return Err(Closed::HolderUnavailable),
            Err(HoldError::Failed) => return Err(Closed::Failed),
        };
        if held.len() != 32 {
            return Err(Closed::Locked);
        }
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&held);
        let file = Self { path, params, key };
        if vault::open(&file.key, &bytes).is_err() {
            return Err(Closed::Locked);
        }
        Ok(file)
    }
}

impl PlatformSecrets for ProtectedFile {
    fn get(&self, _: &str, name: &str) -> Result<Option<String>, String> {
        Ok(self.opened()?.0.get(name).cloned())
    }
    fn set(&self, _: &str, name: &str, value: &str) -> Result<(), String> {
        let mut entries = self.opened()?;
        entries.0.insert(name.into(), value.into());
        self.write(&entries)
    }
    fn delete(&self, _: &str, name: &str) -> Result<(), String> {
        let mut entries = self.opened()?;
        if let Some(mut old) = entries.0.remove(name) {
            old.zeroize();
            self.write(&entries)?;
        }
        Ok(())
    }
}

/// A new, empty protected file under `dir` for `passphrase`, unlocked at once.
/// Caller holds the folder's lock and has checked the passphrase.
pub fn create_protected(env: &Env, dir: &Path, passphrase: &str) -> Result<(), Closed> {
    if !env.holder.available() {
        return Err(Closed::HolderUnavailable);
    }
    let params = Params::new().map_err(|_| Closed::Failed)?;
    let mut key = Zeroizing::new([0u8; 32]);
    params.derive_into(passphrase.as_bytes(), &mut key);
    let file = ProtectedFile {
        path: dir.join(PROTECTED_FILE),
        params,
        key,
    };
    file.write(&Entries::default())
        .map_err(|_| Closed::Failed)?;
    hold(env, dir, &file.key, None)
}

fn hold(env: &Env, dir: &Path, key: &vault::Key, seconds: Option<u32>) -> Result<(), Closed> {
    let name = key_name(dir).map_err(|_| Closed::Failed)?;
    env.holder.put(&name, key, seconds).map_err(|e| match e {
        HoldError::Unavailable => Closed::HolderUnavailable,
        HoldError::Failed => Closed::Failed,
    })
}

/// Opens the file under `dir` with `passphrase` and hands its key to the holder.
/// Caller holds the folder's lock.
pub fn unlock_with(
    env: &Env,
    dir: &Path,
    passphrase: &str,
    seconds: Option<u32>,
) -> Result<(), Closed> {
    if !env.holder.available() {
        return Err(Closed::HolderUnavailable);
    }
    let bytes = read_file(&dir.join(PROTECTED_FILE))?;
    let params = match Params::of(&bytes) {
        Ok(p) => p,
        Err(Unopened::Version(_)) => return Err(Closed::Newer),
        Err(Unopened::Damaged) => return Err(Closed::WrongOrDamaged),
    };
    let mut key = Zeroizing::new([0u8; 32]);
    params.derive_into(passphrase.as_bytes(), &mut key);
    let plain = Zeroizing::new(vault::open(&key, &bytes).map_err(|_| Closed::WrongOrDamaged)?);
    serde_json::from_slice::<Entries>(&plain).map_err(|_| Closed::WrongOrDamaged)?;
    hold(env, dir, &key, seconds)
}

/// The protected file's state, without asking for anything.
pub fn protected_state(env: &Env, dir: &Path) -> Result<(), Closed> {
    if !env.holder.available() {
        return Err(Closed::HolderUnavailable);
    }
    ProtectedFile::unlocked(dir, env.holder.as_ref()).map(|_| ())
}

/// Removes the protected file and forgets its key. No passphrase needed: nothing is read.
/// Caller holds the folder's lock.
pub fn remove_protected(env: &Env, dir: &Path) -> Result<(), &'static str> {
    match std::fs::remove_file(dir.join(PROTECTED_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("state_unavailable"),
    }
    if let Ok(name) = key_name(dir) {
        let _ = env.holder.forget(&name);
    }
    Ok(())
}

/// What the system keyring answers for this folder.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyringState {
    Ready,
    Locked,
    Missing,
}

/// Asks the system keyring without an unlock dialog. Reads the session into memory and drops
/// it; prints nothing. A sign-out still owed is reported as ready without touching it.
pub fn keyring_state(env: &Env, dir: &Path) -> KeyringState {
    let Ok(store) = (env.keyring)(dir) else {
        return KeyringState::Missing;
    };
    if store.sign_out_pending() {
        return KeyringState::Ready;
    }
    match store.try_load() {
        // A missing entry reads as empty even in a locked collection: ask the lock itself.
        Ok(_) => match store.is_locked() {
            Ok(false) => KeyringState::Ready,
            Ok(true) => KeyringState::Locked,
            Err(_) => KeyringState::Missing,
        },
        Err(quince_auth::Error::Keyring(message))
            if message.to_ascii_lowercase().contains("locked") =>
        {
            KeyringState::Locked
        }
        Err(_) => KeyringState::Missing,
    }
}

/// The store `dir`'s chosen storage opens. Caller holds the folder's lock.
pub fn open_store_in(env: &Env, dir: &Path) -> Result<Store, &'static str> {
    match effective_kind(dir)? {
        Kind::SystemKeyring => (env.keyring)(dir).map_err(|_| "secure_storage_unavailable"),
        Kind::ProtectedFile => {
            let file =
                ProtectedFile::unlocked(dir, env.holder.as_ref()).map_err(|c| tool_text(&c))?;
            Store::with_secrets(dir, Box::new(file)).map_err(|_| "secure_storage_unavailable")
        }
    }
}

/// [`open_store_in`] with this process's [`env`]. Caller holds the folder's lock.
pub fn open_store(dir: &Path) -> Result<Store, &'static str> {
    open_store_in(env(), dir)
}

/// What a tool answers for each reason the protected file is closed. Codes, not secrets.
pub fn tool_text(closed: &Closed) -> &'static str {
    match closed {
        Closed::Missing => NOT_SET_UP,
        Closed::Locked => LOCKED,
        Closed::WrongOrDamaged => "protected file damaged: run skykeep-mcp setup in a terminal",
        Closed::HolderUnavailable => "protected file unusable here: this computer blocks the kernel key storage it needs; run skykeep-mcp status in a terminal",
        Closed::Newer => "protected file written by a newer skykeep-mcp: update skykeep-mcp",
        Closed::Failed => "protected file unreadable: run skykeep-mcp status in a terminal",
    }
}

/// Why the chosen protected file can't be opened now, if it is chosen and can't. Never asks
/// for anything.
pub fn protected_problem(dir: &Path) -> Option<Closed> {
    if !matches!(effective_kind(dir), Ok(Kind::ProtectedFile)) {
        return None;
    }
    protected_state(env(), dir).err()
}
