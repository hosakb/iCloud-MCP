//! Where an unlocked protected file's key waits for the server: the Linux kernel's own key
//! storage, in this user's keyring. Never a file, an argument, the environment or a log.
//!
//! The key is a `user` key in the user keyring (`@u`), so the server finds it from any session
//! of the same user, including one started without a desktop. It lasts until the computer
//! restarts, or until the timeout given to `unlock`.
use zeroize::Zeroizing;

#[derive(Debug, PartialEq, Eq)]
pub enum HoldError {
    /// This system blocks or lacks the kernel key storage (for example a container's filter).
    Unavailable,
    /// The kernel refused the request for another reason.
    Failed,
}

/// Holds derived keys by name. The trait lets tests use a stand-in in place of the kernel.
pub trait KeyHolder: Send + Sync {
    /// Whether keys can be held here at all.
    fn available(&self) -> bool;
    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, HoldError>;
    /// Replaces any key under `name`. `seconds`: forget it after this long.
    fn put(&self, name: &str, key: &[u8], seconds: Option<u32>) -> Result<(), HoldError>;
    fn forget(&self, name: &str) -> Result<(), HoldError>;
}

/// The kernel key storage, through the `add_key` and `keyctl` system calls.
pub struct KernelKeyring;

const KEY_SPEC_USER_KEYRING: libc::c_long = -4;
const KEYCTL_GET_KEYRING_ID: libc::c_long = 0;
const KEYCTL_SETPERM: libc::c_long = 5;
const KEYCTL_UNLINK: libc::c_long = 9;
const KEYCTL_SEARCH: libc::c_long = 10;
const KEYCTL_READ: libc::c_long = 11;
const KEYCTL_SET_TIMEOUT: libc::c_long = 15;
/// Possessor: everything. This user: view, read, search and set a timeout. Nobody else: nothing.
const PERMISSIONS: libc::c_long =
    0x3f00_0000 | 0x0001_0000 | 0x0002_0000 | 0x0008_0000 | 0x0020_0000;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn refused(code: i32) -> HoldError {
    if matches!(code, libc::EPERM | libc::ENOSYS | libc::EACCES) {
        HoldError::Unavailable
    } else {
        HoldError::Failed
    }
}

fn c(text: &str) -> Result<std::ffi::CString, HoldError> {
    std::ffi::CString::new(text).map_err(|_| HoldError::Failed)
}

impl KernelKeyring {
    fn search(&self, name: &str) -> Result<Option<libc::c_long>, HoldError> {
        let kind = c("user")?;
        let desc = c(name)?;
        let id = unsafe {
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_SEARCH,
                KEY_SPEC_USER_KEYRING,
                kind.as_ptr(),
                desc.as_ptr(),
                0 as libc::c_long,
            )
        };
        if id >= 0 {
            return Ok(Some(id));
        }
        match errno() {
            // No such key, expired, or revoked: nothing held.
            libc::ENOKEY | libc::EKEYEXPIRED | libc::EKEYREVOKED => Ok(None),
            code => Err(refused(code)),
        }
    }
}

impl KeyHolder for KernelKeyring {
    fn available(&self) -> bool {
        unsafe {
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_GET_KEYRING_ID,
                KEY_SPEC_USER_KEYRING,
                1 as libc::c_long,
            ) >= 0
        }
    }

    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, HoldError> {
        let Some(id) = self.search(name)? else {
            return Ok(None);
        };
        let mut buf = Zeroizing::new(vec![0u8; 64]);
        let n = unsafe {
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_READ,
                id,
                buf.as_mut_ptr(),
                buf.len() as libc::c_long,
            )
        };
        if n < 0 {
            return match errno() {
                libc::ENOKEY | libc::EKEYEXPIRED | libc::EKEYREVOKED => Ok(None),
                code => Err(refused(code)),
            };
        }
        let n = n as usize;
        if n > buf.len() {
            return Err(HoldError::Failed);
        }
        buf.truncate(n);
        Ok(Some(buf))
    }

    fn put(&self, name: &str, key: &[u8], seconds: Option<u32>) -> Result<(), HoldError> {
        let kind = c("user")?;
        let desc = c(name)?;
        let id = unsafe {
            libc::syscall(
                libc::SYS_add_key,
                kind.as_ptr(),
                desc.as_ptr(),
                key.as_ptr(),
                key.len(),
                KEY_SPEC_USER_KEYRING,
            )
        };
        if id < 0 {
            return Err(refused(errno()));
        }
        let ok = |r: libc::c_long| if r < 0 { Err(refused(errno())) } else { Ok(()) };
        let setup = ok(unsafe { libc::syscall(libc::SYS_keyctl, KEYCTL_SETPERM, id, PERMISSIONS) })
            .and_then(|()| match seconds {
                Some(s) => ok(unsafe {
                    libc::syscall(libc::SYS_keyctl, KEYCTL_SET_TIMEOUT, id, s as libc::c_long)
                }),
                None => Ok(()),
            });
        if setup.is_err() {
            // A key with the wrong permissions or no timeout is not left behind.
            unsafe {
                libc::syscall(libc::SYS_keyctl, KEYCTL_UNLINK, id, KEY_SPEC_USER_KEYRING);
            }
        }
        setup
    }

    fn forget(&self, name: &str) -> Result<(), HoldError> {
        let Some(id) = self.search(name)? else {
            return Ok(());
        };
        let r =
            unsafe { libc::syscall(libc::SYS_keyctl, KEYCTL_UNLINK, id, KEY_SPEC_USER_KEYRING) };
        if r < 0 && errno() != libc::ENOKEY {
            return Err(refused(errno()));
        }
        Ok(())
    }
}

/// A stand-in holder that keeps keys in this process only. For tests and for systems where the
/// caller wants nothing held across processes.
#[derive(Default)]
pub struct MemoryHolder {
    keys: std::sync::Mutex<std::collections::HashMap<String, Zeroizing<Vec<u8>>>>,
    /// Behave like a system that blocks the kernel key storage.
    pub blocked: std::sync::atomic::AtomicBool,
}

impl MemoryHolder {
    fn blocked(&self) -> bool {
        self.blocked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl KeyHolder for MemoryHolder {
    fn available(&self) -> bool {
        !self.blocked()
    }
    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, HoldError> {
        if self.blocked() {
            return Err(HoldError::Unavailable);
        }
        Ok(self.keys.lock().unwrap().get(name).cloned())
    }
    fn put(&self, name: &str, key: &[u8], _: Option<u32>) -> Result<(), HoldError> {
        if self.blocked() {
            return Err(HoldError::Unavailable);
        }
        self.keys
            .lock()
            .unwrap()
            .insert(name.into(), Zeroizing::new(key.to_vec()));
        Ok(())
    }
    fn forget(&self, name: &str) -> Result<(), HoldError> {
        if self.blocked() {
            return Err(HoldError::Unavailable);
        }
        self.keys.lock().unwrap().remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inside a container whose filter blocks the key system calls, every call says
    /// "unavailable" and nothing else; where the kernel allows them, a key goes in, comes back
    /// and goes away. The test key expires on its own after a minute either way.
    #[test]
    fn the_kernel_keyring_either_holds_a_key_or_says_it_is_unavailable() {
        let k = KernelKeyring;
        let name = format!("skykeep-mcp-test:{}", uuid::Uuid::new_v4());
        if !k.available() {
            assert_eq!(k.get(&name), Err(HoldError::Unavailable));
            assert_eq!(
                k.put(&name, &[7; 32], Some(60)),
                Err(HoldError::Unavailable)
            );
            return;
        }
        assert_eq!(k.get(&name), Ok(None));
        k.put(&name, &[7; 32], Some(60)).unwrap();
        assert_eq!(
            k.get(&name).unwrap().as_deref().map(|v| v.to_vec()),
            Some(vec![7; 32])
        );
        k.forget(&name).unwrap();
        assert_eq!(k.get(&name), Ok(None));
    }
}
