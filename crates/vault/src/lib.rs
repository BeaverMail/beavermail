// SPDX-License-Identifier: AGPL-3.0-only

//! Message files at rest: one file per message under a data directory, optionally encrypted,
//! with the key held in key slots (PRD 7.5, D12).
//!
//! Three modes, chosen by the user:
//! - [`Mode::Off`]: files are plain RFC 822, readable by any tool.
//! - [`Mode::System`]: encrypted; the key lives in the system credential store and the app opens
//!   without asking.
//! - [`Mode::Passphrase`]: encrypted; the key is derived from a passphrase typed at each start.
//!
//! A random 256-bit **data key** encrypts every file. It is stored only *wrapped* by a **slot**
//! (a key from the credential store, or one derived from the passphrase with Argon2id), so
//! changing between the two encrypted modes, or changing the passphrase, rewraps the key and
//! touches no message. Each encrypted file is
//! `\0BMF | version | key epoch | nonce | XChaCha20-Poly1305(ciphertext)`; the header and the
//! file's own relative path are authenticated, so a file cannot be swapped for another or
//! silently modified. Plain files are only accepted while a mode change is in progress.
//!
//! Failures never fall back to plain text: a vault whose key cannot be opened fails or stays
//! locked. Turning encryption off is an explicit choice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use argon2::{Algorithm, Argon2, Params, Version};
use beavermail_core::MailError;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{AeadCore, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const MAGIC: &[u8; 4] = b"\0BMF";
const VERSION: u8 = 1;
/// Which data key encrypted a file; room for key rotation later.
const EPOCH: u32 = 1;
const HEADER_LEN: usize = 4 + 1 + 4 + 24;
const SLOT_AAD: &[u8] = b"beavermail/slot/v1";
const KEYCHAIN_SERVICE: &str = "beavermail-vault";
/// The shortest passphrase accepted.
pub const MIN_PASSPHRASE: usize = 10;
/// Returned (as [`MailError::Auth`]) while a passphrase-protected vault has not been unlocked.
pub const LOCKED: &str = "the stored mail is locked";

fn err(e: impl std::fmt::Display) -> MailError {
    MailError::Backend(format!("message files: {e}"))
}

/// How the message files are protected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    System,
    Passphrase,
}

/// Where slot keys are kept. The system implementation uses the OS credential store.
pub trait CredentialStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, MailError>;
    fn set(&self, id: &str, secret: &[u8]) -> Result<(), MailError>;
    /// A missing entry is not an error.
    fn remove(&self, id: &str) -> Result<(), MailError>;
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

    fn remove(&self, id: &str) -> Result<(), MailError> {
        match keyring::Entry::new(KEYCHAIN_SERVICE, id).map_err(err)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(err(format!("credential store: {e}"))),
        }
    }
}

/// A credential store in memory, for tests and for runs where nothing may persist.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

impl MemoryStore {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, MailError> {
        Ok(self.map().get(id).cloned())
    }

    fn set(&self, id: &str, secret: &[u8]) -> Result<(), MailError> {
        self.map().insert(id.to_owned(), secret.to_vec());
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<(), MailError> {
        self.map().remove(id);
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

fn default_mode() -> Mode {
    Mode::System
}

#[derive(Serialize, Deserialize, Clone)]
struct KeysFile {
    version: u32,
    /// Files written before modes existed have none: they were encrypted with the system key.
    #[serde(default = "default_mode")]
    mode: Mode,
    /// A mode change is rewriting files, so both plain and encrypted ones may exist.
    #[serde(default)]
    rewriting: bool,
    slots: Vec<Slot>,
}

/// One way to unlock the data key.
#[derive(Serialize, Deserialize, Clone)]
struct Slot {
    /// "credential-store" or "passphrase".
    kind: String,
    id: String,
    /// Hex of nonce followed by the data key sealed with this slot's key.
    wrapped: String,
    /// Passphrase slots: hex salt and Argon2id cost.
    #[serde(default)]
    salt: String,
    #[serde(default)]
    kdf: Option<Kdf>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Kdf {
    memory_kib: u32,
    passes: u32,
    lanes: u32,
}

/// OWASP's recommendation is at least 19 MiB with 2 passes; this is stronger and still takes
/// a fraction of a second.
const KDF: Kdf = Kdf { memory_kib: 64 * 1024, passes: 3, lanes: 1 };

enum Backing {
    Disk(PathBuf),
    Memory(Mutex<HashMap<PathBuf, Vec<u8>>>),
}

struct State {
    mode: Mode,
    /// `None` when the vault is off or still locked.
    key: Option<Zeroizing<[u8; 32]>>,
    rewriting: bool,
}

pub struct Vault {
    backing: Backing,
    credentials: Box<dyn CredentialStore>,
    state: RwLock<State>,
}

fn random_key() -> Zeroizing<[u8; 32]> {
    Zeroizing::new(XChaCha20Poly1305::generate_key(&mut OsRng).into())
}

fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Result<(XNonce, Vec<u8>), MailError> {
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let sealed = XChaCha20Poly1305::new(key.into()).encrypt(&nonce, Payload { msg: plain, aad }).map_err(|_| err("encryption failed"))?;
    Ok((nonce, sealed))
}

fn open_sealed(key: &[u8; 32], aad: &[u8], nonce: &[u8], sealed: &[u8]) -> Result<Vec<u8>, MailError> {
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad })
        .map_err(|_| err("the file is damaged or was not written by this archive"))
}

fn derive(passphrase: &str, salt: &[u8], kdf: Kdf) -> Result<Zeroizing<[u8; 32]>, MailError> {
    let params = Params::new(kdf.memory_kib, kdf.passes, kdf.lanes, Some(32)).map_err(err)?;
    let mut out = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(passphrase.as_bytes(), salt, &mut *out).map_err(err)?;
    Ok(out)
}

fn wrap(slot_key: &[u8; 32], data_key: &[u8; 32]) -> Result<String, MailError> {
    let (nonce, sealed) = seal(slot_key, SLOT_AAD, data_key)?;
    Ok(to_hex(&[nonce.as_slice(), &sealed].concat()))
}

fn unwrap_key(slot_key: &[u8; 32], wrapped: &str) -> Result<Zeroizing<[u8; 32]>, MailError> {
    let wrapped = from_hex(wrapped)?;
    if wrapped.len() < 24 {
        return Err(err("the key file is damaged"));
    }
    let data = Zeroizing::new(open_sealed(slot_key, SLOT_AAD, &wrapped[..24], &wrapped[24..])?);
    let key: [u8; 32] = data.as_slice().try_into().map_err(|_| err("the data key has the wrong size"))?;
    Ok(Zeroizing::new(key))
}

fn new_id() -> String {
    to_hex(&XChaCha20Poly1305::generate_nonce(&mut OsRng)[..8])
}

/// The relative path as it is authenticated in every file: forward slashes on every platform.
fn context(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

impl Vault {
    /// Opens the vault in `dir`. A first run creates a system-key vault. A passphrase vault opens
    /// locked; see [`Vault::unlock`].
    pub fn open(dir: &Path, credentials: Box<dyn CredentialStore>) -> Result<Self, MailError> {
        std::fs::create_dir_all(dir).map_err(err)?;
        restrict(dir, true);
        let path = dir.join("keys");
        let vault = Self { backing: Backing::Disk(dir.to_path_buf()), credentials, state: RwLock::new(State { mode: Mode::System, key: None, rewriting: false }) };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let file: KeysFile = serde_json::from_str(&text).map_err(err)?;
                let key = match file.mode {
                    Mode::Off | Mode::Passphrase => None,
                    Mode::System => Some(vault.unlock_system(&file)?),
                };
                *vault.write_state() = State { mode: file.mode, key, rewriting: file.rewriting };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (key, file) = vault.new_system_slot(&random_key())?;
                vault.persist(&file)?;
                *vault.write_state() = State { mode: Mode::System, key: Some(key), rewriting: false };
            }
            Err(e) => return Err(err(e)),
        }
        Ok(vault)
    }

    /// A system-key vault that keeps nothing on disk and forgets everything at exit.
    pub fn in_memory() -> Self {
        Self {
            backing: Backing::Memory(Mutex::new(HashMap::new())),
            credentials: Box::new(MemoryStore::default()),
            state: RwLock::new(State { mode: Mode::System, key: Some(random_key()), rewriting: false }),
        }
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn mode(&self) -> Mode {
        self.read_state().mode
    }

    /// A passphrase vault that has not been unlocked yet: nothing can be read or written.
    pub fn is_locked(&self) -> bool {
        let state = self.read_state();
        state.mode == Mode::Passphrase && state.key.is_none()
    }

    /// Opens a passphrase vault. A wrong passphrase is [`MailError::Auth`].
    pub fn unlock(&self, passphrase: &str) -> Result<(), MailError> {
        let file = self.read_keys()?;
        let slot = file.slots.iter().find(|s| s.kind == "passphrase").ok_or_else(|| err("this vault has no passphrase slot"))?;
        let key = self.unwrap_with_passphrase(slot, passphrase)?;
        self.write_state().key = Some(key);
        Ok(())
    }

    fn unwrap_with_passphrase(&self, slot: &Slot, passphrase: &str) -> Result<Zeroizing<[u8; 32]>, MailError> {
        let slot_key = derive(passphrase, &from_hex(&slot.salt)?, slot.kdf.unwrap_or(KDF))?;
        unwrap_key(&slot_key, &slot.wrapped).map_err(|_| MailError::Auth("wrong passphrase".into()))
    }

    fn unlock_system(&self, file: &KeysFile) -> Result<Zeroizing<[u8; 32]>, MailError> {
        for slot in file.slots.iter().filter(|s| s.kind == "credential-store") {
            let Some(slot_key) = self.credentials.get(&slot.id)? else { continue };
            let slot_key: [u8; 32] = slot_key.try_into().map_err(|_| err("the stored slot key has the wrong size"))?;
            return unwrap_key(&slot_key, &slot.wrapped);
        }
        Err(err("cannot unlock the archive: the credential store has no key for it"))
    }

    /// A credential-store slot wrapping `data_key`. The slot key is stored first: a keys file
    /// nobody can open is worse than none.
    fn new_system_slot(&self, data_key: &Zeroizing<[u8; 32]>) -> Result<(Zeroizing<[u8; 32]>, KeysFile), MailError> {
        let slot_key = random_key();
        let id = new_id();
        self.credentials.set(&id, &*slot_key)?;
        let wrapped = wrap(&slot_key, data_key)?;
        let slot = Slot { kind: "credential-store".into(), id, wrapped, salt: String::new(), kdf: None };
        Ok((data_key.clone(), KeysFile { version: 1, mode: Mode::System, rewriting: false, slots: vec![slot] }))
    }

    fn passphrase_slot(data_key: &[u8; 32], passphrase: &str) -> Result<Slot, MailError> {
        if passphrase.chars().count() < MIN_PASSPHRASE {
            return Err(MailError::Backend(format!("the passphrase needs at least {MIN_PASSPHRASE} characters")));
        }
        let salt = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let slot_key = derive(passphrase, &salt[..16], KDF)?;
        Ok(Slot { kind: "passphrase".into(), id: new_id(), wrapped: wrap(&slot_key, data_key)?, salt: to_hex(&salt[..16]), kdf: Some(KDF) })
    }

    fn read_keys(&self) -> Result<KeysFile, MailError> {
        let Backing::Disk(dir) = &self.backing else { return Err(err("this vault has no key file")) };
        serde_json::from_str(&std::fs::read_to_string(dir.join("keys")).map_err(err)?).map_err(err)
    }

    fn persist(&self, file: &KeysFile) -> Result<(), MailError> {
        match &self.backing {
            Backing::Disk(dir) => write_atomic(&dir.join("keys"), serde_json::to_string_pretty(file).map_err(err)?.as_bytes()),
            Backing::Memory(_) => Ok(()),
        }
    }

    /// Writes `data` to the file at `path` (relative to the vault), encrypted unless the vault is off.
    pub fn write(&self, path: &Path, data: &[u8]) -> Result<(), MailError> {
        let state = self.read_state();
        let bytes = match (state.mode, &state.key) {
            (Mode::Off, _) => data.to_vec(),
            (_, Some(key)) => encrypt(key, path, data)?,
            (_, None) => return Err(MailError::Auth(LOCKED.into())),
        };
        drop(state);
        self.put(path, bytes)
    }

    /// The decrypted file, or `None` when it does not exist.
    pub fn read(&self, path: &Path) -> Result<Option<Vec<u8>>, MailError> {
        let Some(bytes) = self.get(path)? else { return Ok(None) };
        let state = self.read_state();
        decode(&bytes, path, state.key.as_deref(), state.mode == Mode::Off || state.rewriting).map(Some)
    }

    fn put(&self, path: &Path, bytes: Vec<u8>) -> Result<(), MailError> {
        match &self.backing {
            Backing::Disk(dir) => write_atomic(&dir.join(path), &bytes),
            Backing::Memory(files) => {
                files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(path.to_path_buf(), bytes);
                Ok(())
            }
        }
    }

    fn get(&self, path: &Path) -> Result<Option<Vec<u8>>, MailError> {
        match &self.backing {
            Backing::Disk(dir) => match std::fs::read(dir.join(path)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(err(e)),
            },
            Backing::Memory(files) => Ok(files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(path).cloned()),
        }
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

    /// Every message file, as paths relative to the vault.
    fn all_files(&self) -> Vec<PathBuf> {
        match &self.backing {
            Backing::Disk(dir) => {
                let mut found = Vec::new();
                let mut pending = vec![dir.join("mail")];
                while let Some(folder) = pending.pop() {
                    let Ok(entries) = std::fs::read_dir(&folder) else { continue };
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            pending.push(path);
                        } else if let Ok(relative) = path.strip_prefix(dir) {
                            found.push(relative.to_path_buf());
                        }
                    }
                }
                found
            }
            Backing::Memory(files) => files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).keys().cloned().collect(),
        }
    }

    /// Rewrites every file to `target` (a key, or `None` for plain), reporting (done, total).
    fn rewrite_all(&self, current: Option<&[u8; 32]>, target: Option<&[u8; 32]>, progress: &dyn Fn(usize, usize)) -> Result<(), MailError> {
        let files = self.all_files();
        for (done, path) in files.iter().enumerate() {
            progress(done, files.len());
            let Some(bytes) = self.get(path)? else { continue };
            // Already in the wanted form (a second sweep, or a resumed change): leave it alone.
            if bytes.starts_with(MAGIC) == target.is_some() && !bytes.starts_with(b"BMF\x01") {
                continue;
            }
            let plain = match decode(&bytes, path, current, true) {
                Ok(plain) => plain,
                // A file that cannot be read is dropped: it is downloaded again when needed.
                Err(_) => {
                    self.remove(path)?;
                    continue;
                }
            };
            let out = match target {
                Some(key) => encrypt(key, path, &plain)?,
                None => plain,
            };
            self.put(path, out)?;
        }
        progress(files.len(), files.len());
        Ok(())
    }

    /// Changes how the files are protected, rewriting them when needed. `passphrase` is required for
    /// [`Mode::Passphrase`], and for changing the passphrase of a passphrase vault. Must not be called
    /// on a locked vault. `progress` receives (files done, files total).
    ///
    /// Order is chosen so that a crash never makes a file unreadable: a new key is saved before
    /// files are encrypted with it, and an old one is kept until no file needs it.
    pub fn set_mode(&self, mode: Mode, passphrase: Option<&str>, progress: &dyn Fn(usize, usize)) -> Result<(), MailError> {
        if self.is_locked() {
            return Err(MailError::Auth(LOCKED.into()));
        }
        let (current_mode, current_key) = {
            let state = self.read_state();
            (state.mode, state.key.clone())
        };
        let old_system = if matches!(self.backing, Backing::Disk(_)) { self.read_keys()?.slots.into_iter().filter(|s| s.kind == "credential-store").map(|s| s.id).collect() } else { Vec::new() };
        let passphrase_for = |key: &[u8; 32]| passphrase.ok_or_else(|| MailError::Backend("enter a passphrase".into())).and_then(|p| Self::passphrase_slot(key, p));

        match (current_mode, mode) {
            (Mode::Off, Mode::Off) | (Mode::System, Mode::System) => Ok(()),
            (Mode::Off, new) => {
                let key = random_key();
                let mut file = if new == Mode::System { self.new_system_slot(&key)?.1 } else { KeysFile { version: 1, mode: new, rewriting: false, slots: vec![passphrase_for(&key)?] } };
                file.mode = new;
                file.rewriting = true;
                self.persist(&file)?;
                *self.write_state() = State { mode: new, key: Some(key.clone()), rewriting: true };
                self.rewrite_all(None, Some(&key), progress)?;
                self.finish_rewrite()
            }
            (_, Mode::Off) => {
                let key = current_key.ok_or_else(|| MailError::Auth(LOCKED.into()))?;
                let mut file = self.read_keys()?;
                file.rewriting = true;
                self.persist(&file)?;
                {
                    // From now on new files are plain, while the key stays for reading the old ones.
                    let mut state = self.write_state();
                    state.mode = Mode::Off;
                    state.rewriting = true;
                }
                self.rewrite_all(Some(&key), None, progress)?;
                // A file written by a call that was already under way when the mode changed.
                self.rewrite_all(Some(&key), None, &|_, _| {})?;
                self.persist(&KeysFile { version: 1, mode: Mode::Off, rewriting: false, slots: Vec::new() })?;
                *self.write_state() = State { mode: Mode::Off, key: None, rewriting: false };
                for id in old_system {
                    self.credentials.remove(&id)?;
                }
                Ok(())
            }
            (Mode::System, Mode::Passphrase) | (Mode::Passphrase, Mode::Passphrase) => {
                let key = current_key.ok_or_else(|| MailError::Auth(LOCKED.into()))?;
                let file = KeysFile { version: 1, mode: Mode::Passphrase, rewriting: false, slots: vec![passphrase_for(&key)?] };
                self.persist(&file)?;
                self.write_state().mode = Mode::Passphrase;
                for id in old_system {
                    self.credentials.remove(&id)?;
                }
                Ok(())
            }
            (Mode::Passphrase, Mode::System) => {
                let key = current_key.ok_or_else(|| MailError::Auth(LOCKED.into()))?;
                let (_, file) = self.new_system_slot(&key)?;
                self.persist(&file)?;
                self.write_state().mode = Mode::System;
                Ok(())
            }
        }
    }

    fn finish_rewrite(&self) -> Result<(), MailError> {
        let mut file = self.read_keys()?;
        file.rewriting = false;
        self.persist(&file)?;
        self.write_state().rewriting = false;
        Ok(())
    }

    /// Forgot the passphrase: deletes every stored message and starts over with a system key.
    /// The mail is still on the server and is downloaded again.
    pub fn reset(&self) -> Result<(), MailError> {
        self.remove_dir(Path::new("mail"))?;
        if let Ok(file) = self.read_keys() {
            for slot in file.slots.iter().filter(|s| s.kind == "credential-store") {
                self.credentials.remove(&slot.id)?;
            }
        }
        let (key, file) = self.new_system_slot(&random_key())?;
        self.persist(&file)?;
        *self.write_state() = State { mode: Mode::System, key: Some(key), rewriting: false };
        Ok(())
    }
}

fn encrypt(key: &[u8; 32], path: &Path, data: &[u8]) -> Result<Vec<u8>, MailError> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len() + 16);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&EPOCH.to_be_bytes());
    let aad = [&out[..], context(path).as_bytes()].concat();
    let (nonce, sealed) = seal(key, &aad, data)?;
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// The content of a file as read from disk. A file without the header is plain text, accepted
/// only where `allow_plain` (the vault is off, or a mode change is rewriting files).
fn decode(bytes: &[u8], path: &Path, key: Option<&[u8; 32]>, allow_plain: bool) -> Result<Vec<u8>, MailError> {
    // The header of the first, unreleased format: never mistake its ciphertext for a plain message.
    if bytes.starts_with(b"BMF\x01") {
        return Err(err("a file in an obsolete format"));
    }
    if !bytes.starts_with(MAGIC) {
        return if allow_plain { Ok(bytes.to_vec()) } else { Err(err("a plain file where an encrypted one is expected")) };
    }
    if bytes.len() < HEADER_LEN {
        return Err(err("not a BeaverMail message file"));
    }
    if bytes[4] != VERSION {
        return Err(err(format!("unsupported file version {}", bytes[4])));
    }
    if u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) != EPOCH {
        return Err(err("the file was written with another key"));
    }
    let key = key.ok_or_else(|| MailError::Auth(LOCKED.into()))?;
    let aad = [&bytes[..9], context(path).as_bytes()].concat();
    open_sealed(key, &aad, &bytes[9..HEADER_LEN], &bytes[HEADER_LEN..])
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
    use std::sync::Arc;

    /// A credential store the test can still look into after handing it to the vault.
    #[derive(Clone, Default)]
    struct Shared(Arc<MemoryStore>);

    impl CredentialStore for Shared {
        fn get(&self, id: &str) -> Result<Option<Vec<u8>>, MailError> {
            self.0.get(id)
        }
        fn set(&self, id: &str, secret: &[u8]) -> Result<(), MailError> {
            self.0.set(id, secret)
        }
        fn remove(&self, id: &str) -> Result<(), MailError> {
            self.0.remove(id)
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beavermail-vault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn open(dir: &Path, store: &Shared) -> Vault {
        Vault::open(dir, Box::new(store.clone())).unwrap()
    }

    fn on_disk(dir: &Path, path: &Path) -> Vec<u8> {
        std::fs::read(dir.join(path)).unwrap()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    const NOP: &dyn Fn(usize, usize) = &|_, _| {};

    #[test]
    fn a_file_round_trips_and_is_not_readable_on_disk() {
        let dir = temp_dir("roundtrip");
        let vault = open(&dir, &Shared::default());
        let path = message_path("me@example.org", "INBOX", 7, 42);
        vault.write(&path, b"Subject: secret plans\r\n\r\nbody").unwrap();
        assert_eq!(vault.read(&path).unwrap().as_deref(), Some(&b"Subject: secret plans\r\n\r\nbody"[..]));
        assert!(!contains(&on_disk(&dir, &path), b"secret"), "plain text must not reach the disk");
        assert_eq!(vault.read(Path::new("mail/none")).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_moved_or_changed_file_is_refused() {
        let dir = temp_dir("tamper");
        let vault = open(&dir, &Shared::default());
        let (a, b) = (message_path("a", "INBOX", 1, 1), message_path("a", "INBOX", 1, 2));
        vault.write(&a, b"hello").unwrap();
        std::fs::copy(dir.join(&a), dir.join(&b)).unwrap();
        assert!(vault.read(&b).is_err(), "a file copied to another message's name");
        let mut bytes = on_disk(&dir, &a);
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(dir.join(&a), &bytes).unwrap();
        assert!(vault.read(&a).is_err(), "a modified file");
        std::fs::write(dir.join(&a), b"From: forged\r\n\r\nhi").unwrap();
        assert!(vault.read(&a).is_err(), "a plain file planted in an encrypted vault");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_data_key_comes_back_from_the_key_file() {
        let dir = temp_dir("reopen");
        let store = Shared::default();
        let path = message_path("a", "INBOX", 1, 1);
        open(&dir, &store).write(&path, b"kept").unwrap();
        assert_eq!(open(&dir, &store).read(&path).unwrap().as_deref(), Some(&b"kept"[..]));
        // Without the slot key in the credential store nothing opens, and nothing falls back to plain text.
        assert!(Vault::open(&dir, Box::new(MemoryStore::default())).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_encryption_off_leaves_plain_files_and_no_keys() {
        let dir = temp_dir("off");
        let store = Shared::default();
        let vault = open(&dir, &store);
        let path = message_path("a", "INBOX", 1, 1);
        vault.write(&path, b"Subject: open book").unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        vault.set_mode(Mode::Off, None, &move |done, total| sink.lock().unwrap().push((done, total))).unwrap();
        assert_eq!(on_disk(&dir, &path), b"Subject: open book", "a plain .eml any tool can read");
        assert_eq!(vault.mode(), Mode::Off);
        assert_eq!(seen.lock().unwrap().last(), Some(&(1, 1)));
        assert!(store.0.map().is_empty(), "the slot key left the credential store");
        // It stays off after a restart, and new files are plain too.
        let again = open(&dir, &store);
        again.write(&message_path("a", "INBOX", 1, 2), b"also plain").unwrap();
        assert_eq!(on_disk(&dir, &message_path("a", "INBOX", 1, 2)), b"also plain");
        assert_eq!(again.read(&path).unwrap().as_deref(), Some(&b"Subject: open book"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_written_while_encryption_is_being_removed_end_up_plain_too() {
        let dir = temp_dir("offlive");
        let store = Shared::default();
        let vault = open(&dir, &store);
        for uid in 1..=3 {
            vault.write(&message_path("a", "INBOX", 1, uid), format!("Subject: {uid}").as_bytes()).unwrap();
        }
        let late = message_path("a", "INBOX", 1, 99);
        let progress = |done: usize, _total: usize| {
            if done == 1 {
                vault.write(&late, b"Subject: written during the change").unwrap();
            }
        };
        vault.set_mode(Mode::Off, None, &progress).unwrap();
        for uid in [1, 2, 3, 99] {
            let bytes = on_disk(&dir, &message_path("a", "INBOX", 1, uid));
            assert!(bytes.starts_with(b"Subject: "), "file {uid} is plain");
        }
        assert!(store.0.map().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_of_the_first_unreleased_format_is_never_taken_for_plain_text() {
        let dir = temp_dir("legacy");
        let vault = open(&dir, &Shared::default());
        let path = message_path("a", "INBOX", 1, 1);
        vault.write(&path, b"x").unwrap();
        std::fs::write(dir.join(&path), b"BMF\x01\0\0\0\x01some old ciphertext").unwrap();
        assert!(vault.read(&path).is_err());
        vault.set_mode(Mode::Off, None, NOP).unwrap();
        assert_eq!(vault.read(&path).unwrap(), None, "dropped, to be downloaded again, not kept as garbage");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_encryption_on_encrypts_what_was_already_stored() {
        let dir = temp_dir("on");
        let store = Shared::default();
        let vault = open(&dir, &store);
        vault.set_mode(Mode::Off, None, NOP).unwrap();
        let path = message_path("a", "INBOX", 1, 1);
        vault.write(&path, b"Subject: was plain").unwrap();
        vault.set_mode(Mode::System, None, NOP).unwrap();
        assert!(!contains(&on_disk(&dir, &path), b"was plain"));
        assert_eq!(open(&dir, &store).read(&path).unwrap().as_deref(), Some(&b"Subject: was plain"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_passphrase_vault_opens_locked_and_unlocks_only_with_the_right_one() {
        let dir = temp_dir("passphrase");
        let store = Shared::default();
        let vault = open(&dir, &store);
        let path = message_path("a", "INBOX", 1, 1);
        vault.write(&path, b"Subject: mine").unwrap();
        let before = on_disk(&dir, &path);
        assert!(vault.set_mode(Mode::Passphrase, Some("short"), NOP).is_err(), "too short");
        assert!(vault.set_mode(Mode::Passphrase, None, NOP).is_err(), "missing");
        vault.set_mode(Mode::Passphrase, Some("correct horse battery"), NOP).unwrap();
        assert_eq!(on_disk(&dir, &path), before, "rewrapping the key touches no message");
        assert!(store.0.map().is_empty(), "the system key is gone from the credential store");

        let reopened = open(&dir, &store);
        assert!(reopened.is_locked());
        assert!(matches!(reopened.read(&path), Err(MailError::Auth(_))));
        assert!(matches!(reopened.write(&path, b"x"), Err(MailError::Auth(_))));
        assert!(matches!(reopened.unlock("wrong passphrase!"), Err(MailError::Auth(_))));
        assert!(reopened.is_locked());
        reopened.unlock("correct horse battery").unwrap();
        assert_eq!(reopened.read(&path).unwrap().as_deref(), Some(&b"Subject: mine"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changing_between_the_encrypted_modes_and_the_passphrase_keeps_every_file() {
        let dir = temp_dir("rewrap");
        let store = Shared::default();
        let vault = open(&dir, &store);
        let path = message_path("a", "INBOX", 1, 1);
        vault.write(&path, b"Subject: steady").unwrap();
        let before = on_disk(&dir, &path);
        vault.set_mode(Mode::Passphrase, Some("first passphrase"), NOP).unwrap();
        vault.set_mode(Mode::Passphrase, Some("second passphrase"), NOP).unwrap();
        vault.set_mode(Mode::System, None, NOP).unwrap();
        assert_eq!(on_disk(&dir, &path), before);
        let again = open(&dir, &store);
        assert!(!again.is_locked());
        assert_eq!(again.read(&path).unwrap().as_deref(), Some(&b"Subject: steady"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_the_passphrase_starts_over_without_the_old_mail() {
        let dir = temp_dir("reset");
        let store = Shared::default();
        let vault = open(&dir, &store);
        let path = message_path("a", "INBOX", 1, 1);
        vault.write(&path, b"Subject: lost").unwrap();
        vault.set_mode(Mode::Passphrase, Some("a long passphrase"), NOP).unwrap();
        let locked = open(&dir, &store);
        assert!(locked.is_locked());
        locked.reset().unwrap();
        assert_eq!(locked.mode(), Mode::System);
        assert_eq!(locked.read(&path).unwrap(), None);
        locked.write(&path, b"Subject: new").unwrap();
        assert_eq!(open(&dir, &store).read(&path).unwrap().as_deref(), Some(&b"Subject: new"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_file_is_dropped_by_a_mode_change_instead_of_blocking_it() {
        let dir = temp_dir("broken");
        let vault = open(&dir, &Shared::default());
        let (good, bad) = (message_path("a", "INBOX", 1, 1), message_path("a", "INBOX", 1, 2));
        vault.write(&good, b"fine").unwrap();
        vault.write(&bad, b"broken soon").unwrap();
        let mut bytes = on_disk(&dir, &bad);
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(dir.join(&bad), bytes).unwrap();
        vault.set_mode(Mode::Off, None, NOP).unwrap();
        assert_eq!(vault.read(&good).unwrap().as_deref(), Some(&b"fine"[..]));
        assert_eq!(vault.read(&bad).unwrap(), None, "it will be downloaded again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_writes_of_the_same_data_differ() {
        let vault = Vault::in_memory();
        let path = Path::new("mail/x");
        vault.write(path, b"same").unwrap();
        let first = vault.get(path).unwrap();
        vault.write(path, b"same").unwrap();
        assert_ne!(first, vault.get(path).unwrap(), "a fresh nonce for every file");
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
        vault.write(&message_path("a", "INBOX", 1, 1), b"1").unwrap();
        vault.write(&message_path("a", "Sent", 1, 1), b"2").unwrap();
        vault.remove_dir(&folder_path("a", "INBOX")).unwrap();
        assert_eq!(vault.read(&message_path("a", "INBOX", 1, 1)).unwrap(), None);
        assert!(vault.read(&message_path("a", "Sent", 1, 1)).unwrap().is_some());
        vault.remove_dir(&account_path("a")).unwrap();
        assert_eq!(vault.read(&message_path("a", "Sent", 1, 1)).unwrap(), None);
    }

    #[test]
    fn the_keys_file_holds_no_plain_key_and_old_files_without_a_mode_still_open() {
        let dir = temp_dir("keysfile");
        let store = Shared::default();
        open(&dir, &store);
        let text = std::fs::read_to_string(dir.join("keys")).unwrap();
        assert!(text.contains("credential-store"));
        let slot_key = store.0.map().values().next().cloned().unwrap();
        assert!(!text.contains(&to_hex(&slot_key)), "the slot key stays in the credential store");
        // The format of the first release had no "mode" field.
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value.as_object_mut().unwrap().remove("mode");
        std::fs::write(dir.join("keys"), value.to_string()).unwrap();
        assert_eq!(open(&dir, &store).mode(), Mode::System);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
