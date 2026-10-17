// SPDX-License-Identifier: AGPL-3.0-only

//! Signed and encrypted mail: which form a message takes (S/MIME or OpenPGP), and whether its
//! signature holds. Pure Rust: certificates are checked against the system's authorities with
//! webpki, OpenPGP with rPGP.

mod keys;
mod pgp_mime;
mod smime_own;

pub use smime_own::{CertificateInfo, certificate_info, certificate_path, import_certificate, seal as smime_seal, signer_certificate, unwrap_signed};
/// Opens a message encrypted with S/MIME (see `smime_own::decrypt`).
pub use smime_own::decrypt as decrypt_smime;

pub use pgp_mime::{Opened, add_autocrypt, decrypt, encrypt};
pub use keys::{ImportError, autocrypt_header, backup_password, export, generate, import, key_path, public_key, short_id};

use base64::Engine;
use mail_parser::{MessageParser, MimeHeaders, PartType};
use rustls_pki_types::{CertificateDer, TrustAnchor};

pub use beavermail_core::{Signature, SignatureCheck as Check, SignedWith as Kind};

/// Encrypts a message as `seal` says: OpenPGP or S/MIME.
pub fn seal(raw: &[u8], seal: &beavermail_core::Seal) -> Result<Vec<u8>, String> {
    match seal {
        beavermail_core::Seal::OpenPgp { secret, to, bcc } => encrypt(raw, secret, to, bcc),
        beavermail_core::Seal::Smime { own, to } => smime_seal(raw, own, to),
    }
}

/// The certificate authorities this computer trusts, for S/MIME signers; read once.
pub fn system_roots() -> &'static [TrustAnchor<'static>] {
    static ROOTS: std::sync::OnceLock<Vec<TrustAnchor<'static>>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| rustls_native_certs::load_native_certs().certs.iter().filter_map(|c| webpki::anchor_from_trusted_cert(c).ok().map(|a| a.to_owned())).collect())
}

/// The sender's address, lower case: whose keys to look up for a message.
pub fn sender_of(raw: &[u8]) -> Option<String> {
    sender(&MessageParser::default().parse(raw)?)
}

/// The authorities in a PEM file, in place of the system's (tests, or a company's own authority).
pub fn roots_from_pem(pem: &[u8]) -> Vec<TrustAnchor<'static>> {
    let text = String::from_utf8_lossy(pem);
    text.split("-----BEGIN CERTIFICATE-----")
        .skip(1)
        .filter_map(|block| {
            let body: String = block.split("-----END CERTIFICATE-----").next()?.chars().filter(|c| !c.is_whitespace()).collect();
            let der = base64::engine::general_purpose::STANDARD.decode(body).ok()?;
            webpki::anchor_from_trusted_cert(&CertificateDer::from(der)).ok().map(|a| a.to_owned())
        })
        .collect()
}

/// The key a message's `Autocrypt:` header carries for its sender, as (address, key); none when the
/// header names another address than the sender's.
pub fn autocrypt_key(raw: &[u8]) -> Option<(String, Vec<u8>)> {
    let message = MessageParser::default().parse(raw)?;
    let from = sender(&message)?;
    let header = message.header_raw("Autocrypt")?;
    let (mut addr, mut keydata) = (None, None);
    for attribute in header.split(';') {
        match attribute.trim().split_once('=') {
            Some(("addr", value)) => addr = Some(value.trim().to_lowercase()),
            Some(("keydata", value)) => keydata = Some(value.chars().filter(|c| !c.is_whitespace()).collect::<String>()),
            _ => {}
        }
    }
    let addr = addr.filter(|a| *a == from)?;
    Some((addr, base64::engine::general_purpose::STANDARD.decode(keydata?).ok()?))
}

/// Checks the signature a message carries, if any. `keys` are the OpenPGP keys known for its sender;
/// the key its own Autocrypt header carries counts too, as one first seen in this message.
pub fn verify(raw: &[u8], keys: &[Vec<u8>], roots: &[TrustAnchor<'_>]) -> Option<Signature> {
    let message = MessageParser::default().parse(raw)?;
    let from = sender(&message).unwrap_or_default();
    let root = message.root_part();
    let signed = root.content_type().filter(|c| c.ctype().eq_ignore_ascii_case("multipart") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("signed")));
    let (kind, check) = match signed {
        Some(content_type) => {
            let protocol = content_type.attribute("protocol").unwrap_or_default().to_ascii_lowercase();
            let parts = root.sub_parts()?;
            let (content, signature) = (message.part(*parts.first()?)?, message.part(*parts.get(1)?)?);
            let data = canonical(&message.raw_message()[content.offset_header as usize..content.offset_end as usize]);
            let signature = match &signature.body {
                PartType::Binary(b) | PartType::InlineBinary(b) => b.to_vec(),
                PartType::Text(t) => t.as_bytes().to_vec(),
                _ => return None,
            };
            match protocol.as_str() {
                "application/pkcs7-signature" | "application/x-pkcs7-signature" => (Kind::Smime, smime::check(&data, &signature, roots)),
                "application/pgp-signature" => (Kind::OpenPgp, openpgp::check_detached(&data, &signature, &keys_for(raw, keys))),
                _ => return None,
            }
        }
        // Inline: a cleartext-signed block in the message's text.
        None => {
            let text = message.body_text(0)?;
            let start = text.find("-----BEGIN PGP SIGNED MESSAGE-----")?;
            (Kind::OpenPgp, openpgp::check_cleartext(&text[start..], &keys_for(raw, keys)))
        }
    };
    let check = match check {
        Check::Valid { signer, .. } if !from.is_empty() && !signer.eq_ignore_ascii_case(&from) => Check::NotSender { signer },
        check => check,
    };
    Some(Signature { kind, check })
}

/// The sender's address, lower case.
fn sender(message: &mail_parser::Message<'_>) -> Option<String> {
    message.from()?.first()?.address().map(str::to_lowercase)
}

/// The keys to try: those known, then the one this message's own Autocrypt header carries.
fn keys_for(raw: &[u8], known: &[Vec<u8>]) -> Vec<(Vec<u8>, bool)> {
    let mut keys: Vec<(Vec<u8>, bool)> = known.iter().map(|k| (k.clone(), false)).collect();
    if let Some((_, key)) = autocrypt_key(raw) {
        keys.push((key, true));
    }
    keys
}

/// The signed entity as signed: lines ending in CRLF. The parser's part already ends before the line
/// break that belongs to the boundary (RFC 1847).
fn canonical(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 40);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

pub(crate) mod smime {
    use super::Check;
    use cms::content_info::ContentInfo;
    use cms::signed_data::{SignedData, SignerIdentifier};
    use der::asn1::OctetString;
    use der::{Decode, Encode, oid::ObjectIdentifier};
    use rustls_pki_types::{CertificateDer, SignatureVerificationAlgorithm, TrustAnchor, UnixTime};
    use x509_cert::Certificate;
    use x509_cert::ext::pkix::SubjectAltName;
    use x509_cert::ext::pkix::name::GeneralName;

    const MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
    const EMAIL_ADDRESS: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.1");
    const COMMON_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");
    /// The certificate is for email (id-kp-emailProtection), as DER without tag and length.
    const EMAIL_PROTECTION: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x04];

    pub(crate) fn check(content: &[u8], p7s: &[u8], roots: &[TrustAnchor<'_>]) -> Check {
        let unreadable = || Check::Untrusted("the signature could not be read".into());
        let Ok(info) = ContentInfo::from_der(p7s) else { return unreadable() };
        let Ok(signed) = info.content.decode_as::<SignedData>() else { return unreadable() };
        let certs: Vec<Certificate> = signed
            .certificates
            .iter()
            .flat_map(|set| set.0.iter())
            .filter_map(|c| match c {
                cms::cert::CertificateChoices::Certificate(cert) => Some(cert.clone()),
                _ => None,
            })
            .collect();
        let Some(signer) = signed.signer_infos.0.iter().next() else { return unreadable() };
        let Some(cert) = certs.iter().find(|c| identifies(&signer.sid, c)) else { return Check::Untrusted("the signer's certificate is not in the message".into()) };
        let Some(hash) = Hash::of(&signer.digest_alg.oid) else { return Check::Untrusted("it uses a digest this program does not accept (such as SHA-1)".into()) };
        // The content is signed through its digest among the signed attributes, which are what the signature covers.
        let Some(attributes) = &signer.signed_attrs else { return Check::Untrusted("signatures without signed attributes are not supported".into()) };
        let digest = attributes.iter().find(|a| a.oid == MESSAGE_DIGEST).and_then(|a| a.values.iter().next()).and_then(|v| v.decode_as::<OctetString>().ok());
        if digest.as_ref().map(OctetString::as_bytes) != Some(hash.digest(content).as_ref()) {
            return Check::Changed;
        }
        let (Ok(signed_bytes), Ok(cert_der)) = (attributes.to_der(), cert.to_der()) else { return unreadable() };
        let cert_der = CertificateDer::from(cert_der);
        let Ok(end_entity) = webpki::EndEntityCert::try_from(&cert_der) else { return Check::Untrusted("the signer's certificate could not be read".into()) };
        let sound = hash.algorithms(&signer.signature_algorithm.oid).iter().any(|alg| end_entity.verify_signature(*alg, &signed_bytes, signer.signature.as_bytes()).is_ok());
        if !sound {
            return Check::Changed;
        }
        let intermediates: Vec<CertificateDer<'static>> = certs.iter().filter(|c| *c != cert).filter_map(|c| c.to_der().ok().map(CertificateDer::from)).collect();
        // shortcut: the chain is checked as of now, so a message signed with a certificate that has since
        // expired shows as untrusted; checking at the signing time (a signed attribute) would keep it valid.
        let usage = webpki::KeyUsage::required_if_present(EMAIL_PROTECTION);
        if let Err(e) = end_entity.verify_for_usage(webpki::ALL_VERIFICATION_ALGS, roots, &intermediates, UnixTime::now(), usage, None, None) {
            return Check::Untrusted(match e {
                webpki::Error::CertExpired { .. } => "its certificate has expired".into(),
                webpki::Error::CertNotValidYet { .. } => "its certificate is not valid yet".into(),
                webpki::Error::UnknownIssuer => "its certificate is not from an authority this computer trusts".into(),
                webpki::Error::RequiredEkuNotFoundContext(_) => "its certificate is not meant for email".into(),
                e => format!("its certificate cannot be trusted ({e})"),
            });
        }
        match email_of(cert) {
            Some(signer) => Check::Valid { signer, by: authority(cert) },
            None => Check::Untrusted("its certificate names no email address".into()),
        }
    }

    pub(crate) fn identifies(sid: &SignerIdentifier, cert: &Certificate) -> bool {
        match sid {
            SignerIdentifier::IssuerAndSerialNumber(id) => id.issuer == cert.tbs_certificate.issuer && id.serial_number == cert.tbs_certificate.serial_number,
            SignerIdentifier::SubjectKeyIdentifier(id) => cert
                .tbs_certificate
                .get::<x509_cert::ext::pkix::SubjectKeyIdentifier>()
                .ok()
                .flatten()
                .is_some_and(|(_, own)| own.0 == id.0),
        }
    }

    /// The address the certificate is for: its alternative name, else the old email field of its subject.
    pub(crate) fn email_of(cert: &Certificate) -> Option<String> {
        let alt = cert.tbs_certificate.get::<SubjectAltName>().ok().flatten().and_then(|(_, names)| {
            names.0.iter().find_map(|n| match n {
                GeneralName::Rfc822Name(email) => Some(email.to_string()),
                _ => None,
            })
        });
        alt.or_else(|| field(&cert.tbs_certificate.subject, EMAIL_ADDRESS)).map(|e| e.to_lowercase())
    }

    /// Who issued the certificate, by the common name of the issuer.
    pub(crate) fn authority(cert: &Certificate) -> String {
        field(&cert.tbs_certificate.issuer, COMMON_NAME).unwrap_or_else(|| cert.tbs_certificate.issuer.to_string())
    }

    fn field(name: &x509_cert::name::Name, oid: ObjectIdentifier) -> Option<String> {
        name.0.iter().flat_map(|rdn| rdn.0.iter()).find(|a| a.oid == oid).and_then(|a| {
            a.value
                .decode_as::<der::asn1::Ia5StringRef<'_>>()
                .map(|s| s.to_string())
                .or_else(|_| a.value.decode_as::<der::asn1::Utf8StringRef<'_>>().map(|s| s.to_string()))
                .or_else(|_| a.value.decode_as::<der::asn1::PrintableStringRef<'_>>().map(|s| s.to_string()))
                .ok()
        })
    }

    #[derive(Clone, Copy)]
    enum Hash {
        Sha256,
        Sha384,
        Sha512,
    }

    impl Hash {
        fn of(oid: &ObjectIdentifier) -> Option<Self> {
            match oid.to_string().as_str() {
                "2.16.840.1.101.3.4.2.1" => Some(Self::Sha256),
                "2.16.840.1.101.3.4.2.2" => Some(Self::Sha384),
                "2.16.840.1.101.3.4.2.3" => Some(Self::Sha512),
                _ => None,
            }
        }

        fn digest(self, data: &[u8]) -> ring::digest::Digest {
            let alg = match self {
                Self::Sha256 => &ring::digest::SHA256,
                Self::Sha384 => &ring::digest::SHA384,
                Self::Sha512 => &ring::digest::SHA512,
            };
            ring::digest::digest(alg, data)
        }

        /// The ways a signature with this digest may have been made, by the signature algorithm named:
        /// RSA (plain or with the digest in its name), ECDSA on either curve, or Ed25519.
        fn algorithms(self, signature: &ObjectIdentifier) -> Vec<&'static dyn SignatureVerificationAlgorithm> {
            use webpki::ring as alg;
            let rsa = match self {
                Self::Sha256 => alg::RSA_PKCS1_2048_8192_SHA256,
                Self::Sha384 => alg::RSA_PKCS1_2048_8192_SHA384,
                Self::Sha512 => alg::RSA_PKCS1_2048_8192_SHA512,
            };
            let ecdsa = match self {
                Self::Sha256 => vec![alg::ECDSA_P256_SHA256, alg::ECDSA_P384_SHA256],
                Self::Sha384 => vec![alg::ECDSA_P256_SHA384, alg::ECDSA_P384_SHA384],
                Self::Sha512 => vec![],
            };
            match signature.to_string().as_str() {
                "1.2.840.113549.1.1.1" | "1.2.840.113549.1.1.11" | "1.2.840.113549.1.1.12" | "1.2.840.113549.1.1.13" => vec![rsa],
                "1.2.840.10045.2.1" | "1.2.840.10045.4.3.2" | "1.2.840.10045.4.3.3" | "1.2.840.10045.4.3.4" => ecdsa,
                "1.3.101.112" => vec![alg::ED25519],
                _ => Vec::new(),
            }
        }
    }
}

mod openpgp {
    use super::Check;
    use pgp::composed::{CleartextSignedMessage, Deserializable, SignedPublicKey, StandaloneSignature};
    use pgp::packet::{PublicKey, PublicSubkey};
    use pgp::types::KeyDetails;

    /// A key a signature may have been made with: the primary key or one of its subkeys.
    enum Candidate<'a> {
        Primary(&'a PublicKey),
        Sub(&'a PublicSubkey),
    }

    impl Candidate<'_> {
        /// Whether the signature names this key as its issuer (or names none).
        fn issued(&self, issuer: &str) -> bool {
            let (fingerprint, id) = match self {
                Self::Primary(k) => (k.fingerprint().to_string(), k.key_id().to_string()),
                Self::Sub(k) => (k.fingerprint().to_string(), k.key_id().to_string()),
            };
            issuer.is_empty() || fingerprint.eq_ignore_ascii_case(issuer) || id.eq_ignore_ascii_case(issuer)
        }
    }

    pub fn check_detached(data: &[u8], armored: &[u8], keys: &[(Vec<u8>, bool)]) -> Check {
        let Ok((signature, _)) = StandaloneSignature::from_armor_single(armored) else { return Check::Untrusted("the signature could not be read".into()) };
        verify_with(keys, &signature, |key| match key {
            Candidate::Primary(k) => signature.verify(*k, data).is_ok(),
            Candidate::Sub(k) => signature.verify(*k, data).is_ok(),
        })
    }

    pub fn check_cleartext(text: &str, keys: &[(Vec<u8>, bool)]) -> Check {
        let Ok((message, _)) = CleartextSignedMessage::from_string(text) else { return Check::Untrusted("the signature could not be read".into()) };
        let Some(signature) = message.signatures().first().cloned() else { return Check::Untrusted("the signature could not be read".into()) };
        verify_with(keys, &signature, |key| match key {
            Candidate::Primary(k) => message.verify(*k).is_ok(),
            Candidate::Sub(k) => message.verify(*k).is_ok(),
        })
    }

    /// Tries each key (its primary key and subkeys) the signature names as its issuer: a key that
    /// matches but does not verify means the message changed; no key that matches, an unknown key.
    fn verify_with(keys: &[(Vec<u8>, bool)], signature: &StandaloneSignature, holds: impl Fn(&Candidate<'_>) -> bool) -> Check {
        let issuer = signature.signature.issuer_fingerprint().first().map(|f| f.to_string()).or_else(|| signature.signature.issuer().first().map(|k| k.to_string())).unwrap_or_default();
        let mut issuer_known = false;
        for (bytes, first_seen) in keys {
            let Ok(key) = SignedPublicKey::from_bytes(bytes.as_slice()).or_else(|_| SignedPublicKey::from_armor_single(bytes.as_slice()).map(|(k, _)| k)) else { continue };
            let candidates: Vec<Candidate<'_>> = std::iter::once(Candidate::Primary(&key.primary_key)).chain(key.public_subkeys.iter().map(|s| Candidate::Sub(&s.key))).filter(|c| c.issued(&issuer)).collect();
            if candidates.is_empty() {
                continue;
            }
            issuer_known = true;
            if candidates.iter().any(&holds) {
                let fingerprint = key.primary_key.fingerprint().to_string().to_uppercase();
                let short = &fingerprint[fingerprint.len().saturating_sub(16)..];
                let by = if *first_seen { format!("the key sent with this message ({short})") } else { format!("key {short}") };
                // Only the message's own key holds while another was known: the key changed.
                let known_before = keys.iter().any(|(_, first)| !first);
                return match signer(&key) {
                    Some(signer) if *first_seen && known_before => Check::KeyChanged { signer },
                    Some(signer) => Check::Valid { signer, by },
                    None => Check::Untrusted("its key names no email address".into()),
                };
            }
        }
        if issuer_known { Check::Changed } else { Check::UnknownKey { key: issuer.to_uppercase() } }
    }

    /// The address in the key's first user id that has one ("Ada <ada@example.org>").
    fn signer(key: &SignedPublicKey) -> Option<String> {
        key.details.users.iter().find_map(|u| {
            let id = String::from_utf8_lossy(u.id.id()).into_owned();
            let address = match (id.find('<'), id.rfind('>')) {
                (Some(a), Some(b)) if a < b => id[a + 1..b].to_owned(),
                _ => id,
            };
            address.contains('@').then(|| address.trim().to_lowercase())
        })
    }
}
