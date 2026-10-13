// SPDX-License-Identifier: AGPL-3.0-only

//! Message files at rest: one encrypted file per message under a data directory, with the key
//! held in key slots (PRD 7.5).
//!
//! A random 256-bit **data key** encrypts every file. It is stored only *wrapped* by a **slot**:
//! today one slot, a random key kept in the system credential store. Further slots (passphrase,
//! recovery key) wrap the same data key without touching any file. Each file is
//! `magic | version | key epoch | nonce | XChaCha20-Poly1305(ciphertext)`, with the header and a
//! caller-chosen context (account, folder, UID) as authenticated data, so a file cannot be
//! swapped for another or silently modified.
//!
//! There is no plain-text fallback: if no slot can be opened, [`Vault::open`] fails.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use beavermail_core::MailError;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{AeadCore, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const MAGIC: &[u8; 3] = b"BMF";
const VERSION: u8 = 1;
/// Which data key encrypted a file; room for key rotation later.
const EPOCH: u32 = 1;
const HEADER_LEN: usize = 3 + 1 + 4 + 24;
const SLOT_AAD: &[u8] = b"beavermail/slot/v1";
const KEYCHAIN_SERVICE: &str = "beavermail-vault";

fn err(e: impl std::fmt::Display) -> MailError {
    MailError::Backend(format!("message files: {e}"))
}

/// Where slot keys are kept. The system implementation uses the OS credential store.
pub trait CredentialStore {
    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, MailError>;
    fn set(&self, id: &str, secret: &[u8]) -> Result<(), MailError>;
}

/// Keychain, Credential Manager or Secret Service.
pub struct SystemStore;

impl CredentialStore for SystemStore {
    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, MailError> {
        match keyring::Entry::new(KEYCHAIN_SERVICE, id).map_err(err)?.get_password() {
            Ok(hex) => Ok(Some(from_hex(&hex)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(err(format!("credential store: {e}"))),
        }
    }

    fn set(&self, id: &str, secret: &[u8]) -> Result<(), MailError> {
        keyring::Entry::new(KEYCHAIN_SERVICE, id).map_err(err)?.set_password(&to_hex(secret)).map_err(|e| err(format!("credential store: {e}")))
    }
}

/// A credential store in memory, for tests and for runs where nothing may persist.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

impl CredentialStore for MemoryStore {
    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, MailError> {
        Ok(self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(id).cloned())
    }

    fn set(&self, id: &str, secret: &[u8]) -> Result<(), MailError> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.to_owned(), secret.to_vec());
        Ok(())
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Result<Vec<u8>, MailError> {
    if !text.len().is_multiple_of(2) {
        return Err(err("bad key encoding"));
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| err("bad key encoding"))).collect()
}

#[derive(Serialize, Deserialize)]
struct KeysFile {
    version: u32,
    slots: Vec<Slot>,
}

/// One way to unlock the data key.
#[derive(Serialize, Deserialize)]
struct Slot {
    /// "credential-store" today; "passphrase" and "recovery" later.
    kind: String,
    id: String,
    /// Hex of nonce followed by the data key sealed with this slot's key.
    wrapped: String,
}

enum Backing {
    Disk(PathBuf),
    Memory(Mutex<HashMap<PathBuf, Vec<u8>>>),
}

pub struct Vault {
    backing: Backing,
    key: Zeroizing<[u8; 32]>,
}

fn random_key() -> Zeroizing<[u8; 32]> {
    Zeroizing::new(XChaCha20Poly1305::generate_key(&mut OsRng).into())
}

fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Result<(XNonce, Vec<u8>), MailError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let sealed = cipher.encrypt(&nonce, Payload { msg: plain, aad }).map_err(|_| err("encryption failed"))?;
    Ok((nonce, sealed))
}

fn open_sealed(key: &[u8; 32], aad: &[u8], nonce: &[u8], sealed: &[u8]) -> Result<Vec<u8>, MailError> {
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad })
        .map_err(|_| err("the file is damaged or was not written by this archive"))
}

impl Vault {
    /// Opens the vault in `dir`, creating the key slot on first use.
    pub fn open(dir: &Path, credentials: &dyn CredentialStore) -> Result<Self, MailError> {
        std::fs::create_dir_all(dir).map_err(err)?;
        restrict(dir, true);
        let path = dir.join("keys");
        let key = match std::fs::read_to_string(&path) {
            Ok(text) => unlock(&serde_json::from_str::<KeysFile>(&text).map_err(err)?, credentials)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (key, file) = create(credentials)?;
                write_atomic(&path, serde_json::to_string_pretty(&file).map_err(err)?.as_bytes())?;
                key
            }
            Err(e) => return Err(err(e)),
        };
        Ok(Self { backing: Backing::Disk(dir.to_path_buf()), key })
    }

    /// A vault that keeps nothing on disk and forgets everything at exit.
    pub fn in_memory() -> Self {
        Self { backing: Backing::Memory(Mutex::new(HashMap::new())), key: random_key() }
    }

    /// Encrypts `data` into the file at `path` (relative to the vault). `context` is authenticated
    /// with the file, and must be given again to read it.
    pub fn write(&self, path: &Path, context: &str, data: &[u8]) -> Result<(), MailError> {
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.push(VERSION);
        header.extend_from_slice(&EPOCH.to_be_bytes());
        let aad = [header.as_slice(), context.as_bytes()].concat();
        let (nonce, sealed) = seal(&self.key, &aad, data)?;
        header.extend_from_slice(&nonce);
        header.extend_from_slice(&sealed);
        match &self.backing {
            Backing::Disk(dir) => write_atomic(&dir.join(path), &header),
            Backing::Memory(files) => {
                files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(path.to_path_buf(), header);
                Ok(())
            }
        }
    }

    /// The decrypted file, or `None` when it does not exist.
    pub fn read(&self, path: &Path, context: &str) -> Result<Option<Vec<u8>>, MailError> {
        let bytes = match &self.backing {
            Backing::Disk(dir) => match std::fs::read(dir.join(path)) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(err(e)),
            },
            Backing::Memory(files) => match files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(path) {
                Some(bytes) => bytes.clone(),
                None => return Ok(None),
            },
        };
        if bytes.len() < HEADER_LEN || &bytes[..3] != MAGIC {
            return Err(err("not a BeaverMail message file"));
        }
        if bytes[3] != VERSION {
            return Err(err(format!("unsupported file version {}", bytes[3])));
        }
        if u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) != EPOCH {
            return Err(err("the file was written with another key"));
        }
        let aad = [&bytes[..8], context.as_bytes()].concat();
        open_sealed(&self.key, &aad, &bytes[8..HEADER_LEN], &bytes[HEADER_LEN..]).map(Some)
    }

    /// Deletes one file; a missing file is not an error.
    pub fn remove(&self, path: &Path) -> Result<(), MailError> {
        match &self.backing {
            Backing::Disk(dir) => match std::fs::remove_file(dir.join(path)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
                _ => Ok(()),
            },
            Backing::Memory(files) => {
                files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(path);
                Ok(())
            }
        }
    }

    /// Deletes a directory and everything below it; a missing one is not an error.
    pub fn remove_dir(&self, path: &Path) -> Result<(), MailError> {
        match &self.backing {
            Backing::Disk(dir) => match std::fs::remove_dir_all(dir.join(path)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
                _ => Ok(()),
            },
            Backing::Memory(files) => {
                files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|p, _| !p.starts_with(path));
                Ok(())
            }
        }
    }
}

/// A name that is safe as one path component and never `.` or `..`: everything but ASCII
/// letters, digits, `-`, `_`, `@` and `+` is written as `%XX`, so different names stay different.
pub fn component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'@' | b'+') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.is_empty() { "%".to_owned() } else { out }
}

/// The path of a message file: `mail/<account>/<folder>/<uidvalidity>-<uid>.eml`.
pub fn message_path(account: &str, folder: &str, uidvalidity: u32, uid: u32) -> PathBuf {
    Path::new("mail").join(component(account)).join(component(folder)).join(format!("{uidvalidity}-{uid}.eml"))
}

/// The directory holding a folder's files.
pub fn folder_path(account: &str, folder: &str) -> PathBuf {
    Path::new("mail").join(component(account)).join(component(folder))
}

/// The directory holding all of an account's files.
pub fn account_path(account: &str) -> PathBuf {
    Path::new("mail").join(component(account))
}

fn create(credentials: &dyn CredentialStore) -> Result<(Zeroizing<[u8; 32]>, KeysFile), MailError> {
    let data_key = random_key();
    let slot_key = random_key();
    let id = to_hex(&XChaCha20Poly1305::generate_nonce(&mut OsRng)[..8]);
    // The slot key goes to the credential store first: a keys file nobody can open is worse than none.
    credentials.set(&id, &*slot_key)?;
    let (nonce, sealed) = seal(&slot_key, SLOT_AAD, &*data_key)?;
    let wrapped = to_hex(&[nonce.as_slice(), &sealed].concat());
    Ok((data_key, KeysFile { version: 1, slots: vec![Slot { kind: "credential-store".into(), id, wrapped }] }))
}

fn unlock(file: &KeysFile, credentials: &dyn CredentialStore) -> Result<Zeroizing<[u8; 32]>, MailError> {
    for slot in file.slots.iter().filter(|s| s.kind == "credential-store") {
        let Some(slot_key) = credentials.get(&slot.id)? else { continue };
        let slot_key: [u8; 32] = slot_key.try_into().map_err(|_| err("the stored slot key has the wrong size"))?;
        let wrapped = from_hex(&slot.wrapped)?;
        if wrapped.len() < 24 {
            return Err(err("the key file is damaged"));
        }
        let data = Zeroizing::new(open_sealed(&slot_key, SLOT_AAD, &wrapped[..24], &wrapped[24..])?);
        let key: [u8; 32] = data.as_slice().try_into().map_err(|_| err("the data key has the wrong size"))?;
        return Ok(Zeroizing::new(key));
    }
    Err(err("cannot unlock the archive: no key slot could be opened (the credential store has no key for this archive)"))
}

/// Writes through a temporary file, so a crash never leaves a half-written file under the real name.
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), MailError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
        restrict(dir, true);
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(err)?;
    restrict(&tmp, false);
    std::fs::rename(&tmp, path).map_err(err)
}

/// Owner-only permissions where the platform has them.
fn restrict(path: &Path, directory: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }));
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beavermail-vault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_file_round_trips_and_is_not_readable_on_disk() {
        let dir = temp_dir("roundtrip");
        let store = MemoryStore::default();
        let vault = Vault::open(&dir, &store).unwrap();
        let path = message_path("me@example.org", "INBOX", 7, 42);
        vault.write(&path, "ctx", b"Subject: secret plans\r\n\r\nbody").unwrap();
        assert_eq!(vault.read(&path, "ctx").unwrap().as_deref(), Some(&b"Subject: secret plans\r\n\r\nbody"[..]));
        let on_disk = std::fs::read(dir.join(&path)).unwrap();
        assert!(!on_disk.windows(6).any(|w| w == b"secret"), "plain text must not reach the disk");
        assert_eq!(vault.read(Path::new("mail/none"), "ctx").unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_context_or_a_changed_byte_is_refused() {
        let dir = temp_dir("tamper");
        let vault = Vault::open(&dir, &MemoryStore::default()).unwrap();
        let path = message_path("a", "INBOX", 1, 1);
        vault.write(&path, "a/INBOX/1", b"hello").unwrap();
        assert!(vault.read(&path, "a/INBOX/2").is_err(), "a file read under another identity");
        let mut bytes = std::fs::read(dir.join(&path)).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(dir.join(&path), &bytes).unwrap();
        assert!(vault.read(&path, "a/INBOX/1").is_err(), "a modified file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_data_key_comes_back_from_the_key_file() {
        let dir = temp_dir("reopen");
        let store = MemoryStore::default();
        let first = Vault::open(&dir, &store).unwrap();
        let path = message_path("a", "INBOX", 1, 1);
        first.write(&path, "c", b"kept").unwrap();
        let second = Vault::open(&dir, &store).unwrap();
        assert_eq!(second.read(&path, "c").unwrap().as_deref(), Some(&b"kept"[..]));
        // Without the slot key in the credential store nothing opens, and nothing falls back to plain text.
        assert!(Vault::open(&dir, &MemoryStore::default()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_writes_of_the_same_data_differ() {
        let vault = Vault::in_memory();
        let path = Path::new("mail/x");
        vault.write(path, "c", b"same").unwrap();
        let Backing::Memory(files) = &vault.backing else { unreachable!() };
        let first = files.lock().unwrap().get(path).cloned().unwrap();
        vault.write(path, "c", b"same").unwrap();
        let second = files.lock().unwrap().get(path).cloned().unwrap();
        assert_ne!(first, second, "a fresh nonce for every file");
    }

    #[test]
    fn names_become_safe_distinct_path_components() {
        assert_eq!(component("INBOX"), "INBOX");
        assert_eq!(component("[Gmail]/Sent Mail"), "%5BGmail%5D%2FSent%20Mail");
        assert_eq!(component(".."), "%2E%2E");
        assert_ne!(component("a/b"), component("a%2Fb"));
        assert_eq!(component(""), "%");
    }

    #[test]
    fn removing_a_folder_removes_its_files_only() {
        let vault = Vault::in_memory();
        vault.write(&message_path("a", "INBOX", 1, 1), "c", b"1").unwrap();
        vault.write(&message_path("a", "Sent", 1, 1), "c", b"2").unwrap();
        vault.remove_dir(&folder_path("a", "INBOX")).unwrap();
        assert_eq!(vault.read(&message_path("a", "INBOX", 1, 1), "c").unwrap(), None);
        assert!(vault.read(&message_path("a", "Sent", 1, 1), "c").unwrap().is_some());
        vault.remove_dir(&account_path("a")).unwrap();
        assert_eq!(vault.read(&message_path("a", "Sent", 1, 1), "c").unwrap(), None);
    }

    #[test]
    fn keys_file_holds_no_plain_key() {
        let dir = temp_dir("keysfile");
        let store = MemoryStore::default();
        Vault::open(&dir, &store).unwrap();
        let text = std::fs::read_to_string(dir.join("keys")).unwrap();
        assert!(text.contains("credential-store"));
        let slot_key = store.0.lock().unwrap().values().next().cloned().unwrap();
        assert!(!text.contains(&to_hex(&slot_key)), "the slot key stays in the credential store");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
