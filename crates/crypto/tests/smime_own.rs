// SPDX-License-Identifier: AGPL-3.0-only

//! The user's S/MIME certificate: imported from a `.p12`, opening mail encrypted with OpenSSL, and
//! sealing mail that OpenSSL can open.

use beavermail_crypto::{Check, ImportError, Opened, certificate_info, decrypt_smime, import_certificate, roots_from_pem, signer_certificate, smime_seal, unwrap_signed};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn ada() -> Vec<u8> {
    import_certificate(&fixture("ada.p12"), Some("p12pass")).unwrap()
}

#[test]
fn a_certificate_comes_in_with_its_password_in_either_protection() {
    assert_eq!(import_certificate(&fixture("ada.p12"), None), Err(ImportError::NeedsPassword));
    assert_eq!(import_certificate(&fixture("ada.p12"), Some("nope")), Err(ImportError::WrongPassword));
    let info = certificate_info(&ada()).unwrap();
    assert_eq!((info.address.as_str(), info.issuer.as_str()), ("ada@example.org", "BeaverMail Test CA"));
    assert!(info.expires.starts_with('2'), "{info:?}");
    assert!(import_certificate(&fixture("ada-legacy.p12"), Some("p12pass")).is_ok(), "the older 3DES/RC2 protection, as Windows makes");
}

#[test]
fn mail_encrypted_by_openssl_opens_whatever_the_cipher() {
    for name in ["smime-encrypted-aes256.eml", "smime-encrypted-aes128.eml", "smime-encrypted-des3.eml", "smime-encrypted-oaep.eml"] {
        match decrypt_smime(&fixture(name), &[ada()]) {
            Some(Opened::Message(raw, _)) => {
                let raw = String::from_utf8_lossy(&raw);
                assert!(raw.contains("Subject: Secret lunch") && raw.contains("See you at noon."), "{name}: {raw}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    assert!(matches!(decrypt_smime(&fixture("smime-encrypted-aes256.eml"), &[]), Some(Opened::NoKey)));
    assert!(decrypt_smime(&fixture("smime-signed.eml"), &[ada()]).is_none(), "signed, not encrypted");
}

#[test]
fn a_message_signed_in_one_piece_is_unwrapped_and_checked() {
    let roots = roots_from_pem(&fixture("test-ca.pem"));
    let (inner, signature) = unwrap_signed(&fixture("smime-opaque.eml"), &roots).unwrap();
    assert!(String::from_utf8_lossy(&inner).contains("See you at noon."));
    assert!(matches!(signature.check, Check::Valid { signer, .. } if signer == "ada@example.org"));
    assert!(signer_certificate(&fixture("smime-opaque.eml")).is_some());
    assert!(signer_certificate(&fixture("smime-signed.eml")).is_some(), "and from a detached signature");
}

#[test]
fn a_sealed_message_opens_with_the_recipients_certificate_and_its_signature_holds() {
    let raw = b"From: Ada Lovelace <ada@example.org>\r\nTo: Ada <ada@example.org>\r\nSubject: Note\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSealed by BeaverMail.\r\n";
    let cert = signer_certificate(&fixture("smime-signed.eml")).unwrap();
    let sealed = smime_seal(raw, &ada(), &[cert]).unwrap();
    let text = String::from_utf8_lossy(&sealed);
    assert!(text.contains("application/pkcs7-mime; smime-type=enveloped-data") && text.contains("Subject: Note") && !text.contains("Sealed by BeaverMail"));
    let Some(Opened::Message(opened, _)) = decrypt_smime(&sealed, &[ada()]) else { panic!("does not open") };
    let roots = roots_from_pem(&fixture("test-ca.pem"));
    let signature = beavermail_crypto::verify(&opened, &[], &roots).unwrap();
    assert!(matches!(&signature.check, Check::Valid { signer, .. } if signer == "ada@example.org"), "{signature:?}");
    if let Ok(dir) = std::env::var("INTEROP_DIR") {
        std::fs::write(format!("{dir}/smime-sealed.eml"), &sealed).unwrap();
    }
}
