//! A passphrase-protected file of secrets, for computers with no usable system keyring.
//!
//! The file is a header and the sealed contents:
//!
//! ```text
//! "SKPF"  version (1)  kdf (1 = PBKDF2-HMAC-SHA256)  iterations (u32, big-endian)  salt (16)
//! nonce (12 bytes, fresh every write)  ciphertext and tag (AES-256-GCM)
//! ```
//!
//! The whole header is bound in as associated data, so a changed iteration count or salt fails
//! to open just as a changed byte of the contents does. The version and kdf bytes let later
//! builds change the parameters; a file of any layout this build does not know is not guessed
//! at. A wrong passphrase and a damaged file look the same from here, on purpose.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::Hmac;
use sha2::Sha256;

pub const MAGIC: &[u8; 4] = b"SKPF";
pub const VERSION: u8 = 1;
const KDF_PBKDF2_SHA256: u8 = 1;
/// The fewest PBKDF2 rounds a file may ask for, and the count new files get.
pub const MIN_ITERATIONS: u32 = 600_000;
/// The most: a file asking for more would only burn the CPU.
pub const MAX_ITERATIONS: u32 = 10_000_000;
const SALT: usize = 16;
const HEADER: usize = MAGIC.len() + 1 + 1 + 4 + SALT;
const NONCE: usize = 12;
const SMALLEST: usize = HEADER + NONCE + 16;

/// A derived key: 256 bits.
pub type Key = [u8; 32];

/// How a file's key is derived from its passphrase. Not secret: it is written in the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub iterations: u32,
    pub salt: [u8; SALT],
}

/// Why a file did not open.
#[derive(Debug, PartialEq, Eq)]
pub enum Unopened {
    /// Written by a layout this build does not know.
    Version(u8),
    /// Cut short, changed, or sealed under another passphrase.
    Damaged,
}

fn fill(bytes: &mut [u8]) -> std::io::Result<()> {
    use rand::TryRngCore;
    rand::rngs::OsRng
        .try_fill_bytes(bytes)
        .map_err(std::io::Error::other)
}

impl Params {
    /// Parameters for a new file: a random salt and the minimum round count.
    pub fn new() -> std::io::Result<Self> {
        let mut salt = [0u8; SALT];
        fill(&mut salt)?;
        Ok(Self {
            iterations: MIN_ITERATIONS,
            salt,
        })
    }

    /// The parameters a file was written with.
    pub fn of(bytes: &[u8]) -> Result<Self, Unopened> {
        if bytes.len() < MAGIC.len() + 1 || !bytes.starts_with(MAGIC) {
            return Err(Unopened::Damaged);
        }
        if bytes[MAGIC.len()] != VERSION {
            return Err(Unopened::Version(bytes[MAGIC.len()]));
        }
        if bytes.len() < SMALLEST || bytes[MAGIC.len() + 1] != KDF_PBKDF2_SHA256 {
            return Err(Unopened::Damaged);
        }
        let at = MAGIC.len() + 2;
        let iterations = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
            return Err(Unopened::Damaged);
        }
        let mut salt = [0u8; SALT];
        salt.copy_from_slice(&bytes[at + 4..HEADER]);
        Ok(Self { iterations, salt })
    }

    /// The key for `passphrase`. The caller clears both when done.
    #[must_use]
    pub fn derive(&self, passphrase: &[u8]) -> Key {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2::<Hmac<Sha256>>(passphrase, &self.salt, self.iterations, &mut key)
            .expect("pbkdf2 with a 32-byte output never fails");
        key
    }

    fn header(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(KDF_PBKDF2_SHA256);
        out.extend_from_slice(&self.iterations.to_be_bytes());
        out.extend_from_slice(&self.salt);
        out
    }
}

/// `plain` sealed under `key`, with the header of `params` and a nonce of its own.
pub fn seal(key: &Key, params: &Params, plain: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE];
    fill(&mut nonce)?;
    let mut out = params.header();
    let sealed = Aes256Gcm::new(key.into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: &out,
            },
        )
        .map_err(|_| std::io::Error::other("couldn't seal the file"))?;
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// What [`seal`] wrote, opened, only if every byte is as it was written and `key` is its key.
pub fn open(key: &Key, bytes: &[u8]) -> Result<Vec<u8>, Unopened> {
    Params::of(bytes)?;
    let (header, rest) = bytes.split_at(HEADER);
    let (nonce, sealed) = rest.split_at(NONCE);
    Aes256Gcm::new(key.into())
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: header,
            },
        )
        .map_err(|_| Unopened::Damaged)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &[u8] = br#"{"session":"SYNTHETIC-TOKEN-SENTINEL"}"#;

    #[test]
    fn opens_with_the_right_passphrase_only_and_hides_the_contents() {
        let params = Params::new().unwrap();
        assert_eq!(params.iterations, 600_000);
        let key = params.derive(b"correct horse battery");
        let sealed = seal(&key, &params, PLAIN).unwrap();
        assert!(!sealed.windows(9).any(|w| w == b"SYNTHETIC"));
        assert_eq!(Params::of(&sealed).unwrap(), params);
        assert_eq!(open(&key, &sealed).unwrap(), PLAIN);
        let wrong = params.derive(b"correct horse battery!");
        assert_eq!(open(&wrong, &sealed), Err(Unopened::Damaged));
        assert_ne!(seal(&key, &params, PLAIN).unwrap(), sealed, "fresh nonce");
        let other = Params::new().unwrap();
        assert_ne!(other.salt, params.salt, "random salt");
    }

    #[test]
    fn any_changed_byte_and_any_cut_fail_the_same_way() {
        let params = Params::new().unwrap();
        let key = [7u8; 32];
        let sealed = seal(&key, &params, PLAIN).unwrap();
        for i in MAGIC.len() + 1..sealed.len() {
            let mut bent = sealed.clone();
            bent[i] ^= 0x01;
            assert_eq!(open(&key, &bent), Err(Unopened::Damaged), "byte {i}");
        }
        for n in 0..sealed.len() {
            assert!(open(&key, &sealed[..n]).is_err(), "cut at {n}");
        }
    }

    #[test]
    fn unknown_versions_and_out_of_range_rounds_are_refused() {
        let params = Params::new().unwrap();
        let key = [7u8; 32];
        let mut sealed = seal(&key, &params, PLAIN).unwrap();
        sealed[4] = 2;
        assert_eq!(open(&key, &sealed), Err(Unopened::Version(2)));
        let low = Params {
            iterations: MIN_ITERATIONS - 1,
            salt: params.salt,
        };
        let sealed = seal(&key, &low, PLAIN).unwrap();
        assert_eq!(Params::of(&sealed), Err(Unopened::Damaged));
        let high = Params {
            iterations: MAX_ITERATIONS + 1,
            salt: params.salt,
        };
        assert_eq!(
            Params::of(&seal(&key, &high, PLAIN).unwrap()),
            Err(Unopened::Damaged)
        );
    }
}
