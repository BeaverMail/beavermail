// SPDX-License-Identifier: AGPL-3.0-only

//! The user's own OpenPGP key: made here, or imported; kept in the vault unprotected (the vault
//! encrypts it); exported only under a password.

use base64::Engine;
use pgp::composed::{ArmorOptions, Deserializable, KeyType, SecretKeyParamsBuilder, SignedPublicKey, SignedSecretKey, SubkeyParamsBuilder};
use pgp::crypto::ecc_curve::ECCCurve;
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::ser::Serialize;
use pgp::types::{CompressionAlgorithm, KeyDetails, Password};
use smallvec::smallvec;

/// Where an address's own key is kept in the vault: under `keyring`, which the vault re-encrypts with
/// the mail (not `keys`, the vault's own key file).
pub fn key_path(address: &str) -> std::path::PathBuf {
    let safe: String = address.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || "@._-+".contains(c) { c } else { '_' }).collect();
    std::path::Path::new("keyring").join("openpgp").join(format!("{safe}.key"))
}

/// A new key for `name <address>`: Ed25519 to sign, Curve25519 to receive, in the v4 format every
/// OpenPGP program reads (the newer v6 is not read by GnuPG). It never expires: losing access is a
/// larger risk for a user who did not choose a key than a key outliving its use.
pub fn generate(name: &str, address: &str) -> Result<Vec<u8>, String> {
    let rng = rand::rngs::OsRng;
    let user = if name.trim().is_empty() { address.trim().to_owned() } else { format!("{} <{}>", name.trim(), address.trim()) };
    let subkey = SubkeyParamsBuilder::default().key_type(KeyType::ECDH(ECCCurve::Curve25519)).can_encrypt(true).passphrase(None).build().map_err(|e| e.to_string())?;
    let params = SecretKeyParamsBuilder::default()
        .key_type(KeyType::Ed25519Legacy)
        .can_certify(true)
        .can_sign(true)
        .primary_user_id(user)
        .passphrase(None)
        .preferred_symmetric_algorithms(smallvec![SymmetricKeyAlgorithm::AES256, SymmetricKeyAlgorithm::AES128])
        .preferred_hash_algorithms(smallvec![HashAlgorithm::Sha256, HashAlgorithm::Sha512])
        .preferred_compression_algorithms(smallvec![CompressionAlgorithm::ZLIB, CompressionAlgorithm::ZIP])
        .subkey(subkey)
        .build()
        .map_err(|e| e.to_string())?;
    let key = params.generate(rng).map_err(|e| e.to_string())?.sign(rng, &Password::empty()).map_err(|e| e.to_string())?;
    key.to_bytes().map_err(|e| e.to_string())
}

/// The public half of a secret key, to give out (Autocrypt).
pub fn public_key(secret: &[u8]) -> Option<Vec<u8>> {
    secret_key(secret)?.signed_public_key().to_bytes().ok()
}

/// "1AD8 7E58 A740 1C0E": the end of the key's fingerprint, grouped, for people to compare.
pub fn short_id(key: &[u8]) -> Option<String> {
    let fingerprint = match secret_key(key) {
        Some(secret) => secret.primary_key.fingerprint().to_string(),
        None => public(key)?.primary_key.fingerprint().to_string(),
    }
    .to_uppercase();
    let tail = &fingerprint[fingerprint.len().saturating_sub(16)..];
    Some(tail.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" "))
}

/// A strong password for a backup, made here: 36 digits in groups of four, as Autocrypt's backup
/// code (about 119 bits), easy to read out and type.
pub fn backup_password() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let random = SystemRandom::new();
    let mut groups = Vec::with_capacity(9);
    while groups.len() < 9 {
        let mut two = [0u8; 2];
        if random.fill(&mut two).is_err() {
            continue;
        }
        let n = u16::from_be_bytes(two);
        // Below 60000 only, so each group of four digits is equally likely.
        if n < 60000 {
            groups.push(format!("{:04}", n % 10000));
        }
    }
    groups.join("-")
}

/// The key, armored and locked with `password` (iterated and salted SHA-256, AES: what GnuPG and
/// every OpenPGP program can open), for a backup or another device.
pub fn export(secret: &[u8], password: &str) -> Result<String, String> {
    let mut key = secret_key(secret).ok_or("the key could not be read")?;
    let rng = rand::rngs::OsRng;
    let password = Password::from(password);
    key.primary_key.set_password(rng, &password).map_err(|e| e.to_string())?;
    for sub in &mut key.secret_subkeys {
        sub.key.set_password(rng, &password).map_err(|e| e.to_string())?;
    }
    key.to_armored_string(ArmorOptions::default()).map_err(|e| e.to_string())
}

/// Why a key could not be imported.
#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The file is not an OpenPGP secret key (perhaps a public one).
    NotASecretKey,
    /// It is locked: ask for its password.
    NeedsPassword,
    WrongPassword,
}

/// A secret key from a file (armored or binary), unlocked with `password` when it is locked, ready
/// to keep in the vault.
pub fn import(file: &[u8], password: Option<&str>) -> Result<Vec<u8>, ImportError> {
    let mut key = SignedSecretKey::from_armor_single(file).map(|(k, _)| k).or_else(|_| SignedSecretKey::from_bytes(file)).map_err(|_| ImportError::NotASecretKey)?;
    let locked = key.primary_key.secret_params().is_encrypted() || key.secret_subkeys.iter().any(|s| s.key.secret_params().is_encrypted());
    if locked {
        let password = Password::from(password.ok_or(ImportError::NeedsPassword)?);
        key.primary_key.remove_password(&password).map_err(|_| ImportError::WrongPassword)?;
        for sub in &mut key.secret_subkeys {
            sub.key.remove_password(&password).map_err(|_| ImportError::WrongPassword)?;
        }
    }
    key.to_bytes().map_err(|_| ImportError::NotASecretKey)
}

/// The `Autocrypt:` header value that gives the key out with each message: the sender's address,
/// a wish for encrypted replies, and the key folded into lines.
pub fn autocrypt_header(address: &str, public: &[u8]) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(public);
    let folded: Vec<&str> = data.as_bytes().chunks(72).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    format!("addr={}; prefer-encrypt=mutual; keydata=\r\n {}", address.trim().to_lowercase(), folded.join("\r\n "))
}

pub(crate) fn secret_key(bytes: &[u8]) -> Option<SignedSecretKey> {
    SignedSecretKey::from_bytes(bytes).ok().or_else(|| SignedSecretKey::from_armor_single(bytes).ok().map(|(k, _)| k))
}

pub(crate) fn public(bytes: &[u8]) -> Option<SignedPublicKey> {
    SignedPublicKey::from_bytes(bytes).ok().or_else(|| SignedPublicKey::from_armor_single(bytes).ok().map(|(k, _)| k))
}
