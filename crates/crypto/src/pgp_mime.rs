// SPDX-License-Identifier: AGPL-3.0-only

//! Messages encrypted with OpenPGP as PGP/MIME (RFC 3156): sealing one before it is sent, opening one
//! that arrives.

use mail_parser::{MessageParser, MimeHeaders, PartType};
use pgp::composed::{ArmorOptions, Message, MessageBuilder};
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::public_key::PublicKeyAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::types::{CompressionAlgorithm, KeyDetails, Password};

use crate::keys::{public, secret_key};
use crate::{Check, Kind, Signature};

/// A header of the content (type, encoding, disposition): it moves inside when the message is sealed.
fn content_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("content-")
}

/// The message as (outer header lines, the content's own header lines, body), each header line with
/// its continuations.
pub(crate) fn split(raw: &[u8]) -> (Vec<String>, Vec<String>, Vec<u8>) {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, i + 4)).or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2))).unwrap_or((raw.len(), raw.len()));
    let head = String::from_utf8_lossy(&raw[..end.0]);
    let mut lines: Vec<String> = Vec::new();
    for line in head.lines() {
        match lines.last_mut() {
            Some(last) if line.starts_with([' ', '\t']) => {
                last.push_str("\r\n");
                last.push_str(line);
            }
            _ => lines.push(line.to_owned()),
        }
    }
    let (inner, outer): (Vec<String>, Vec<String>) = lines.into_iter().partition(|l| l.split(':').next().is_some_and(content_header));
    (outer, inner, raw[end.1..].to_vec())
}

/// Adds the `Autocrypt:` header that hands the sender's key out.
pub fn add_autocrypt(raw: &[u8], address: &str, public: &[u8]) -> Vec<u8> {
    let mut out = format!("Autocrypt: {}\r\n", crate::keys::autocrypt_header(address, public)).into_bytes();
    out.extend_from_slice(raw);
    out
}

/// A random boundary that cannot occur in base64 or armored text.
fn boundary() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 12];
    let _ = SystemRandom::new().fill(&mut bytes);
    format!("=-{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// The key of `key` that takes encrypted mail: its encryption subkey, else the primary key when that
/// can encrypt (an old RSA key).
fn encryption_key(key: &pgp::composed::SignedPublicKey) -> Option<EncryptTo<'_>> {
    let encrypts = |alg: PublicKeyAlgorithm| matches!(alg, PublicKeyAlgorithm::ECDH | PublicKeyAlgorithm::X25519 | PublicKeyAlgorithm::X448 | PublicKeyAlgorithm::RSA | PublicKeyAlgorithm::RSAEncrypt | PublicKeyAlgorithm::ElgamalEncrypt);
    key.public_subkeys.iter().find(|s| encrypts(s.key.algorithm())).map(|s| EncryptTo::Sub(&s.key)).or_else(|| encrypts(key.primary_key.algorithm()).then_some(EncryptTo::Primary(&key.primary_key)))
}

enum EncryptTo<'a> {
    Primary(&'a pgp::packet::PublicKey),
    Sub(&'a pgp::packet::PublicSubkey),
}

/// Seals a message as PGP/MIME: its content (and its own headers) encrypted to each of `to` and, unseen
/// by the others, `bcc`, signed inside by `secret`. The outer headers (From, To, Subject…) stay readable.
pub fn encrypt(raw: &[u8], secret: &[u8], to: &[Vec<u8>], bcc: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let signer = secret_key(secret).ok_or("your key could not be read")?;
    let (outer, inner_headers, body) = split(raw);
    let mut inner = inner_headers.join("\r\n").into_bytes();
    inner.extend_from_slice(b"\r\n\r\n");
    inner.extend_from_slice(&body);
    let (to_keys, bcc_keys): (Vec<_>, Vec<_>) = (to.iter().map(|k| public(k).ok_or("a recipient's key could not be read")).collect::<Result<_, _>>()?, bcc.iter().map(|k| public(k).ok_or("a recipient's key could not be read")).collect::<Result<_, _>>()?);
    let rng = rand::rngs::OsRng;
    let mut builder = MessageBuilder::from_bytes("", inner).seipd_v1(rng, SymmetricKeyAlgorithm::AES256);
    builder.compression(CompressionAlgorithm::ZLIB);
    for (keys, anonymous) in [(&to_keys, false), (&bcc_keys, true)] {
        for key in keys.iter() {
            let target = encryption_key(key).ok_or_else(|| format!("{} cannot receive encrypted mail", key.primary_key.fingerprint()))?;
            let result = match (target, anonymous) {
                (EncryptTo::Primary(k), false) => builder.encrypt_to_key(rng, k).map(|_| ()),
                (EncryptTo::Sub(k), false) => builder.encrypt_to_key(rng, k).map(|_| ()),
                (EncryptTo::Primary(k), true) => builder.encrypt_to_key_anonymous(rng, k).map(|_| ()),
                (EncryptTo::Sub(k), true) => builder.encrypt_to_key_anonymous(rng, k).map(|_| ()),
            };
            result.map_err(|e| e.to_string())?;
        }
    }
    builder.sign(&signer.primary_key, Password::empty(), HashAlgorithm::Sha256);
    let armored = builder.to_armored_string(rng, ArmorOptions::default()).map_err(|e| e.to_string())?;
    let b = boundary();
    let mut out = outer.join("\r\n");
    out.push_str(&format!("\r\nContent-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"{b}\"\r\n\r\n"));
    out.push_str("This is an OpenPGP/MIME encrypted message (RFC 3156).\r\n");
    out.push_str(&format!("--{b}\r\nContent-Type: application/pgp-encrypted\r\nContent-Description: PGP/MIME version identification\r\n\r\nVersion: 1\r\n\r\n"));
    out.push_str(&format!("--{b}\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\nContent-Description: OpenPGP encrypted message\r\nContent-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n"));
    out.push_str(&armored.replace("\r\n", "\n").replace('\n', "\r\n"));
    out.push_str(&format!("\r\n--{b}--\r\n"));
    Ok(out.into_bytes())
}

/// What became of an encrypted message.
#[derive(Debug)]
pub enum Opened {
    /// The message as it was before it was sealed: the outer headers with the decrypted content, and
    /// what a signature made inside the encryption says.
    Message(Vec<u8>, Option<Signature>),
    /// None of the user's keys opens it.
    NoKey,
    Failed(String),
}

/// Opens a message encrypted with OpenPGP, as PGP/MIME or inline, with the user's `secrets`; `senders`
/// are the keys known for the sender, to check a signature inside. `None` when it is not encrypted.
pub fn decrypt(raw: &[u8], secrets: &[Vec<u8>], senders: &[Vec<u8>]) -> Option<Opened> {
    let message = MessageParser::default().parse(raw)?;
    let root = message.root_part();
    let encrypted_mime = root.content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("multipart") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("encrypted")) && c.attribute("protocol").is_some_and(|p| p.eq_ignore_ascii_case("application/pgp-encrypted")));
    let (armored, inline) = if encrypted_mime {
        let part = message.part(*root.sub_parts()?.get(1)?)?;
        let text = match &part.body {
            PartType::Binary(b) | PartType::InlineBinary(b) => String::from_utf8_lossy(b).into_owned(),
            PartType::Text(t) => t.to_string(),
            _ => return Some(Opened::Failed("the encrypted part could not be read".into())),
        };
        (text, false)
    } else {
        let text = message.body_text(0)?;
        let start = text.find("-----BEGIN PGP MESSAGE-----")?;
        let end = text.find("-----END PGP MESSAGE-----").map_or(text.len(), |e| e + "-----END PGP MESSAGE-----".len());
        (text[start..end].to_owned(), true)
    };
    let Ok((sealed, _)) = Message::from_string(&armored) else { return Some(Opened::Failed("the encrypted message could not be read".into())) };
    // The sender's key may come with the message itself (Autocrypt).
    let mut known: Vec<pgp::composed::SignedPublicKey> = senders.iter().filter_map(|k| public(k)).collect();
    if let Some((_, key)) = crate::autocrypt_key(raw) {
        known.extend(public(&key));
    }
    for secret in secrets.iter().filter_map(|s| secret_key(s)) {
        let Ok((sealed, _)) = Message::from_string(&armored) else { continue };
        let Ok(opened) = sealed.decrypt(&Password::empty(), &secret) else { continue };
        let Ok(mut plain) = opened.decompress() else { return Some(Opened::Failed("the message could not be decompressed".into())) };
        let Ok(data) = plain.as_data_vec() else { return Some(Opened::Failed("the decrypted message could not be read".into())) };
        let signature = plain.is_signed().then(|| signed_inside(&plain, &known, &message));
        let content = if inline { [b"Content-Type: text/plain; charset=utf-8\r\n\r\n".as_slice(), &data].concat() } else { data };
        return Some(Opened::Message(rebuild(raw, &content), signature));
    }
    drop(sealed);
    Some(Opened::NoKey)
}

/// What a signature inside the encryption says, checked with the keys known for the sender.
fn signed_inside(plain: &Message<'_>, known: &[pgp::composed::SignedPublicKey], message: &mail_parser::Message<'_>) -> Signature {
    let from = message.from().and_then(|f| f.first()).and_then(|a| a.address()).unwrap_or_default().to_lowercase();
    for key in known {
        let holds = plain.verify(&key.primary_key).is_ok() || key.public_subkeys.iter().any(|s| plain.verify(&s.key).is_ok());
        if holds {
            let address = key.details.users.iter().find_map(|u| {
                let id = String::from_utf8_lossy(u.id.id()).into_owned();
                let a = id.find('<').zip(id.rfind('>')).filter(|(a, b)| a < b).map_or(id.clone(), |(a, b)| id[a + 1..b].to_owned());
                a.contains('@').then(|| a.trim().to_lowercase())
            });
            let fingerprint = key.primary_key.fingerprint().to_string().to_uppercase();
            let short = &fingerprint[fingerprint.len().saturating_sub(16)..];
            let check = match address {
                Some(signer) if !from.is_empty() && signer != from => Check::NotSender { signer },
                Some(signer) => Check::Valid { signer, by: format!("key {short}") },
                None => Check::Untrusted("its key names no email address".into()),
            };
            return Signature { kind: Kind::OpenPgp, check };
        }
    }
    Signature { kind: Kind::OpenPgp, check: Check::UnknownKey { key: String::new() } }
}

/// The outer headers of `raw` (without its content headers) followed by the decrypted `content`, which
/// brings its own content headers: a message as if it had never been sealed.
pub(crate) fn rebuild(raw: &[u8], content: &[u8]) -> Vec<u8> {
    let (outer, _, _) = split(raw);
    let mut out = outer.join("\r\n").into_bytes();
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(content);
    out
}
