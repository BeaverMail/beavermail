// SPDX-License-Identifier: AGPL-3.0-only

//! The user's own S/MIME certificate (imported from a `.p12`), and messages encrypted or wrapped
//! with S/MIME: opened when they arrive, sealed before they are sent.

use base64::Engine;
use cms::builder::{ContentEncryptionAlgorithm, EnvelopedDataBuilder, KeyEncryptionInfo, KeyTransRecipientInfoBuilder, SignedDataBuilder, SignerInfoBuilder};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::ContentInfo;
use cms::enveloped_data::{EnvelopedData, RecipientIdentifier, RecipientInfo};
use cms::signed_data::{EncapsulatedContentInfo, SignedData, SignerIdentifier};
use der::asn1::OctetString;
use der::oid::ObjectIdentifier;
use der::referenced::OwnedToRef;
use der::{Decode, Encode};
use mail_parser::{MessageParser, MimeHeaders, PartType};
use rsa::pkcs8::DecodePrivateKey;
use x509_cert::Certificate;
use x509_cert::spki::AlgorithmIdentifierOwned;

use crate::pgp_mime::{Opened, rebuild, split};
use crate::{Check, ImportError, Kind, Signature};

const ENVELOPED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.3");
const AUTH_ENVELOPED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.1.23");
const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
const RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const RSAES_OAEP: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.7");
const SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");

/// Where an address's own certificate and key are kept in the vault.
pub fn certificate_path(address: &str) -> std::path::PathBuf {
    let safe: String = address.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || "@._-+".contains(c) { c } else { '_' }).collect();
    std::path::Path::new("keyring").join("smime").join(format!("{safe}.pem"))
}

/// A certificate with its key from a `.p12`/`.pfx` file, unlocked with `password` (tried empty first),
/// as kept in the vault: the key and the chain, in PEM.
pub fn import_certificate(file: &[u8], password: Option<&str>) -> Result<Vec<u8>, ImportError> {
    let store = match p12_keystore::KeyStore::from_pkcs12(file, password.unwrap_or("")) {
        Ok(store) => store,
        Err(_) if password.is_none() => return Err(ImportError::NeedsPassword),
        Err(_) => return Err(ImportError::WrongPassword),
    };
    let (_, chain) = store.private_key_chain().ok_or(ImportError::NotASecretKey)?;
    let mut pem = pem_block("PRIVATE KEY", chain.key());
    for cert in chain.chain() {
        pem.push_str(&pem_block("CERTIFICATE", cert.as_der()));
    }
    Ok(pem.into_bytes())
}

fn pem_block(label: &str, der: &[u8]) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = data.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// The DER blocks of `label` in a PEM text, in order.
fn blocks(pem: &[u8], label: &str) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(pem);
    let (begin, end) = (format!("-----BEGIN {label}-----"), format!("-----END {label}-----"));
    text.split(&begin)
        .skip(1)
        .filter_map(|b| base64::engine::general_purpose::STANDARD.decode(b.split(&end).next()?.chars().filter(|c| !c.is_whitespace()).collect::<String>()).ok())
        .collect()
}

/// The user's certificate as kept: its key, and its chain, the certificate itself first.
struct Own {
    key: rsa::RsaPrivateKey,
    chain: Vec<Certificate>,
}

fn own(stored: &[u8]) -> Option<Own> {
    let key = rsa::RsaPrivateKey::from_pkcs8_der(blocks(stored, "PRIVATE KEY").first()?).ok()?;
    let chain: Vec<Certificate> = blocks(stored, "CERTIFICATE").iter().filter_map(|d| Certificate::from_der(d).ok()).collect();
    (!chain.is_empty()).then_some(Own { key, chain })
}

/// What a kept certificate is: the address it is for, who issued it, and until when it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateInfo {
    pub address: String,
    pub issuer: String,
    pub expires: String,
}

/// What the user's kept certificate (or a peer's, in DER) says about itself.
pub fn certificate_info(stored: &[u8]) -> Option<CertificateInfo> {
    let cert = own(stored).map(|o| o.chain[0].clone()).or_else(|| Certificate::from_der(stored).ok())?;
    let expires = cert.tbs_certificate.validity.not_after.to_unix_duration().as_secs();
    let expires = chrono_free_date(expires);
    Some(CertificateInfo { address: crate::smime::email_of(&cert).unwrap_or_default(), issuer: crate::smime::authority(&cert), expires })
}

/// "2027-10-17" from seconds since the Unix epoch, without a calendar library.
fn chrono_free_date(secs: u64) -> String {
    // Days to civil date (Howard Hinnant's algorithm).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The pkcs7-mime part of an S/MIME message and its `smime-type`, if the message is one.
fn pkcs7_mime(raw: &[u8]) -> Option<(Vec<u8>, String)> {
    let message = MessageParser::default().parse(raw)?;
    let root = message.root_part();
    let content_type = root.content_type()?;
    let subtype = content_type.subtype().unwrap_or_default().to_ascii_lowercase();
    if !content_type.ctype().eq_ignore_ascii_case("application") || !(subtype == "pkcs7-mime" || subtype == "x-pkcs7-mime") {
        return None;
    }
    let kind = content_type.attribute("smime-type").unwrap_or_default().to_ascii_lowercase();
    let data = match &root.body {
        PartType::Binary(b) | PartType::InlineBinary(b) => b.to_vec(),
        PartType::Text(t) => t.as_bytes().to_vec(),
        _ => return None,
    };
    Some((data, kind))
}

/// Opens a message encrypted with S/MIME with the user's certificates. `None` when it is not one.
pub fn decrypt(raw: &[u8], certificates: &[Vec<u8>]) -> Option<Opened> {
    let (p7m, kind) = pkcs7_mime(raw)?;
    let Ok(info) = ContentInfo::from_der(&p7m) else { return (kind.contains("enveloped")).then(|| Opened::Failed("the encrypted message could not be read".into())) };
    if info.content_type == AUTH_ENVELOPED_DATA {
        return Some(Opened::Failed("it uses AES-GCM encryption, which BeaverMail cannot open yet".into()));
    }
    if info.content_type != ENVELOPED_DATA {
        return None; // signed, not encrypted
    }
    let Ok(enveloped) = info.content.decode_as::<EnvelopedData>() else { return Some(Opened::Failed("the encrypted message could not be read".into())) };
    let mine: Vec<Own> = certificates.iter().filter_map(|c| own(c)).collect();
    for recipient in enveloped.recip_infos.0.iter() {
        let RecipientInfo::Ktri(ktri) = recipient else { continue };
        let Some(owner) = mine.iter().find(|o| names(&ktri.rid, &o.chain[0])) else { continue };
        let Some(content_key) = unwrap_key(&owner.key, &ktri.key_enc_alg, ktri.enc_key.as_bytes()) else { return Some(Opened::Failed("the message's key could not be decrypted".into())) };
        let content = &enveloped.encrypted_content;
        let Some(sealed) = &content.encrypted_content else { return Some(Opened::Failed("the encrypted message has no content".into())) };
        return Some(match decrypt_content(&content.content_enc_alg, &content_key, sealed.as_bytes()) {
            Some(plain) => Opened::Message(rebuild(raw, &plain), None),
            None => Opened::Failed("it uses an encryption BeaverMail cannot open".into()),
        });
    }
    Some(Opened::NoKey)
}

/// Whether a recipient entry names this certificate.
fn names(rid: &RecipientIdentifier, cert: &Certificate) -> bool {
    match rid {
        RecipientIdentifier::IssuerAndSerialNumber(id) => id.issuer == cert.tbs_certificate.issuer && id.serial_number == cert.tbs_certificate.serial_number,
        RecipientIdentifier::SubjectKeyIdentifier(id) => cert.tbs_certificate.get::<x509_cert::ext::pkix::SubjectKeyIdentifier>().ok().flatten().is_some_and(|(_, own)| own.0 == id.0),
    }
}

/// The message's content key, decrypted with the user's RSA key (PKCS #1 v1.5, or OAEP with SHA-1 or SHA-256).
fn unwrap_key(key: &rsa::RsaPrivateKey, algorithm: &AlgorithmIdentifierOwned, wrapped: &[u8]) -> Option<Vec<u8>> {
    if algorithm.oid == RSA_ENCRYPTION {
        // shortcut: the rsa crate's PKCS #1 v1.5 decryption is not constant-time (RUSTSEC-2023-0071); a
        // mail client decrypting its own mail offers no timing oracle to a remote attacker. Revisit if the
        // crate gains a fixed version.
        return key.decrypt(rsa::Pkcs1v15Encrypt, wrapped).ok();
    }
    if algorithm.oid == RSAES_OAEP {
        let sha256 = algorithm.parameters.as_ref().and_then(|p| p.to_der().ok()).is_some_and(|der| der.windows(SHA256.as_bytes().len()).any(|w| w == SHA256.as_bytes()));
        return if sha256 { key.decrypt(rsa::Oaep::new::<sha2::Sha256>(), wrapped).ok() } else { key.decrypt(rsa::Oaep::new::<sha1::Sha1>(), wrapped).ok() };
    }
    None
}

/// The content, decrypted with the content key: AES-CBC, or Triple DES for old senders.
fn decrypt_content(algorithm: &AlgorithmIdentifierOwned, key: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    let iv = algorithm.parameters.as_ref()?.decode_as::<OctetString>().ok()?;
    let iv = iv.as_bytes();
    match algorithm.oid.to_string().as_str() {
        "2.16.840.1.101.3.4.1.2" => cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv).ok()?.decrypt_padded_vec_mut::<Pkcs7>(sealed).ok(),
        "2.16.840.1.101.3.4.1.22" => cbc::Decryptor::<aes::Aes192>::new_from_slices(key, iv).ok()?.decrypt_padded_vec_mut::<Pkcs7>(sealed).ok(),
        "2.16.840.1.101.3.4.1.42" => cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv).ok()?.decrypt_padded_vec_mut::<Pkcs7>(sealed).ok(),
        "1.2.840.113549.3.7" => cbc::Decryptor::<des::TdesEde3>::new_from_slices(key, iv).ok()?.decrypt_padded_vec_mut::<Pkcs7>(sealed).ok(),
        _ => None,
    }
}

/// A message signed with S/MIME in one piece (the content inside the signature): the content as a
/// message of its own, and what the signature says. `None` for any other message.
pub fn unwrap_signed(raw: &[u8], roots: &[rustls_pki_types::TrustAnchor<'_>]) -> Option<(Vec<u8>, Signature)> {
    let (p7m, _) = pkcs7_mime(raw)?;
    let info = ContentInfo::from_der(&p7m).ok()?;
    if info.content_type != SIGNED_DATA {
        return None;
    }
    let signed = info.content.decode_as::<SignedData>().ok()?;
    let content = signed.encap_content_info.econtent.as_ref()?.decode_as::<OctetString>().ok()?;
    let mut check = crate::smime::check(content.as_bytes(), &p7m, roots);
    let message = rebuild(raw, content.as_bytes());
    if let Check::Valid { signer, .. } = &check
        && let Some(from) = crate::sender_of(raw)
        && !signer.eq_ignore_ascii_case(&from)
    {
        check = Check::NotSender { signer: signer.clone() };
    }
    Some((message, Signature { kind: Kind::Smime, check }))
}

/// The certificate an S/MIME signature in the message was made with, in DER: to encrypt to its owner later.
pub fn signer_certificate(raw: &[u8]) -> Option<Vec<u8>> {
    let message = MessageParser::default().parse(raw)?;
    let root = message.root_part();
    let p7 = match pkcs7_mime(raw) {
        Some((p7m, _)) => p7m,
        None => {
            let parts = root.sub_parts()?;
            match &message.part(*parts.get(1)?)?.body {
                PartType::Binary(b) | PartType::InlineBinary(b) => b.to_vec(),
                _ => return None,
            }
        }
    };
    let signed = ContentInfo::from_der(&p7).ok()?.content.decode_as::<SignedData>().ok()?;
    let signer = signed.signer_infos.0.iter().next()?;
    signed.certificates.iter().flat_map(|set| set.0.iter()).find_map(|c| match c {
        CertificateChoices::Certificate(cert) if crate::smime::identifies(&signer.sid, cert) => cert.to_der().ok(),
        _ => None,
    })
}

/// Seals a message with S/MIME: its content (and its own headers) signed with the user's certificate,
/// then encrypted to each recipient's certificate and the user's own. The outer headers stay readable.
pub fn seal(raw: &[u8], stored: &[u8], recipients: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let owner = own(stored).ok_or("your certificate could not be read")?;
    let (outer, inner_headers, body) = split(raw);
    let mut inner = inner_headers.join("\r\n").into_bytes();
    inner.extend_from_slice(b"\r\n\r\n");
    inner.extend_from_slice(&body);
    let signed = sign_detached(&owner, &inner)?;
    let mut certs: Vec<Certificate> = recipients.iter().map(|d| Certificate::from_der(d).map_err(|_| "a recipient's certificate could not be read".to_owned())).collect::<Result<_, _>>()?;
    certs.push(owner.chain[0].clone());
    // One generator per recipient, borrowed by the builder until it is built, so declared before it.
    let mut rngs: Vec<rand::rngs::OsRng> = vec![rand::rngs::OsRng; certs.len()];
    let mut rng = rand::rngs::OsRng;
    let mut builder = EnvelopedDataBuilder::new(None, &signed, ContentEncryptionAlgorithm::Aes256Cbc, None).map_err(|e| e.to_string())?;
    for (cert, rng) in certs.iter().zip(rngs.iter_mut()) {
        let public = rsa::RsaPublicKey::try_from(cert.tbs_certificate.subject_public_key_info.owned_to_ref()).map_err(|_| "only RSA certificates can receive encrypted mail here".to_owned())?;
        let rid = RecipientIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber { issuer: cert.tbs_certificate.issuer.clone(), serial_number: cert.tbs_certificate.serial_number.clone() });
        builder.add_recipient_info(KeyTransRecipientInfoBuilder::new(rid, KeyEncryptionInfo::Rsa(public), rng).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    }
    let enveloped = builder.build_with_rng(&mut rng).map_err(|e| e.to_string())?;
    let info = ContentInfo { content_type: ENVELOPED_DATA, content: der::Any::encode_from(&enveloped).map_err(|e| e.to_string())? };
    let der = info.to_der().map_err(|e| e.to_string())?;
    let mut out = outer.join("\r\n");
    out.push_str("\r\nContent-Type: application/pkcs7-mime; smime-type=enveloped-data; name=\"smime.p7m\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7m\"\r\n\r\n");
    out.push_str(&wrapped_base64(&der));
    Ok(out.into_bytes())
}

/// `entity` signed as multipart/signed (the signature detached from it), the form every client reads.
fn sign_detached(owner: &Own, entity: &[u8]) -> Result<Vec<u8>, String> {
    let digest = ring::digest::digest(&ring::digest::SHA256, entity);
    let content = EncapsulatedContentInfo { econtent_type: DATA, econtent: None };
    let signer = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(owner.key.clone());
    let cert = &owner.chain[0];
    let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber { issuer: cert.tbs_certificate.issuer.clone(), serial_number: cert.tbs_certificate.serial_number.clone() });
    let digest_algorithm = AlgorithmIdentifierOwned { oid: SHA256, parameters: None };
    let info = SignerInfoBuilder::new(&signer, sid, digest_algorithm.clone(), &content, Some(digest.as_ref())).map_err(|e| e.to_string())?;
    let mut builder = SignedDataBuilder::new(&content);
    builder.add_digest_algorithm(digest_algorithm).map_err(|e| e.to_string())?;
    for cert in &owner.chain {
        builder.add_certificate(CertificateChoices::Certificate(cert.clone())).map_err(|e| e.to_string())?;
    }
    let signed = builder.add_signer_info::<rsa::pkcs1v15::SigningKey<sha2::Sha256>, rsa::pkcs1v15::Signature>(info).map_err(|e| e.to_string())?.build().map_err(|e| e.to_string())?;
    let p7s = signed.to_der().map_err(|e| e.to_string())?;
    let boundary = format!("=-smime-{:x}", ring::digest::digest(&ring::digest::SHA256, &p7s).as_ref()[..6].iter().fold(0u64, |a, b| (a << 8) | u64::from(*b)));
    let mut out = format!("Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=sha-256; boundary=\"{boundary}\"\r\n\r\nThis is an S/MIME signed message.\r\n--{boundary}\r\n").into_bytes();
    out.extend_from_slice(entity);
    out.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7s\"\r\n\r\n").as_bytes());
    out.extend_from_slice(wrapped_base64(&p7s).as_bytes());
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok(out)
}

fn wrapped_base64(data: &[u8]) -> String {
    let text = base64::engine::general_purpose::STANDARD.encode(data);
    text.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect::<Vec<_>>().join("\r\n")
}
