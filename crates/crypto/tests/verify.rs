// SPDX-License-Identifier: AGPL-3.0-only

//! Signed messages made with OpenSSL and GnuPG, with throwaway keys and a test authority.

use beavermail_crypto::{Check, Kind, Signature, autocrypt_key, roots_from_pem, verify};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn check(name: &str) -> Signature {
    let roots = roots_from_pem(&fixture("test-ca.pem"));
    verify(&fixture(name), &[], &roots).unwrap_or_else(|| panic!("{name} is signed"))
}

#[test]
fn an_smime_signature_from_a_trusted_authority_holds() {
    assert_eq!(check("smime-signed.eml"), Signature { kind: Kind::Smime, check: Check::Valid { signer: "ada@example.org".into(), by: "BeaverMail Test CA".into() } });
}

#[test]
fn a_changed_smime_message_is_caught() {
    assert_eq!(check("smime-tampered.eml").check, Check::Changed);
}

#[test]
fn an_smime_signature_by_someone_else_than_the_sender_is_said_so() {
    assert_eq!(check("smime-not-sender.eml").check, Check::NotSender { signer: "ada@example.org".into() });
}

#[test]
fn an_smime_certificate_from_an_unknown_authority_is_not_trusted() {
    let signature = verify(&fixture("smime-signed.eml"), &[], &[]).unwrap();
    assert!(matches!(&signature.check, Check::Untrusted(why) if why.contains("authority")), "{signature:?}");
}

#[test]
fn a_pgp_signature_holds_with_the_key_sent_along() {
    let signature = check("pgp-signed.eml");
    assert_eq!(signature.kind, Kind::OpenPgp);
    assert!(matches!(&signature.check, Check::Valid { signer, by } if signer == "ada@example.org" && by.starts_with("the key sent with this message")), "{signature:?}");
}

#[test]
fn a_known_pgp_key_is_named_as_known() {
    let (_, key) = autocrypt_key(&fixture("pgp-signed.eml")).unwrap();
    let signature = verify(&fixture("pgp-signed.eml"), &[key], &[]).unwrap();
    assert!(matches!(&signature.check, Check::Valid { by, .. } if by.starts_with("key ")), "{signature:?}");
}

#[test]
fn a_changed_pgp_message_is_caught() {
    assert_eq!(check("pgp-tampered.eml").check, Check::Changed);
}

#[test]
fn a_pgp_signature_by_an_unknown_key_says_which() {
    assert!(matches!(check("pgp-other-key.eml").check, Check::UnknownKey { key } if !key.is_empty()));
}

#[test]
fn an_inline_pgp_signature_holds() {
    assert!(matches!(check("pgp-inline.eml").check, Check::Valid { signer, .. } if signer == "ada@example.org"));
}

#[test]
fn a_message_without_a_signature_has_none() {
    let plain = b"From: a@example.org\r\nSubject: hi\r\nContent-Type: text/plain\r\n\r\nHello\r\n";
    assert_eq!(verify(plain, &[], &[]), None);
}

#[test]
fn a_message_stored_with_bare_line_feeds_still_verifies() {
    let lf: Vec<u8> = String::from_utf8(fixture("smime-signed.eml")).unwrap().replace("\r\n", "\n").into_bytes();
    let roots = roots_from_pem(&fixture("test-ca.pem"));
    assert!(matches!(verify(&lf, &[], &roots).unwrap().check, Check::Valid { .. }));
}

#[test]
fn a_signature_that_holds_only_with_a_new_key_says_the_key_changed() {
    // Eve's key was the one known for the sender; the message brings Ada's and is signed with it.
    let signature = verify(&fixture("pgp-signed.eml"), &[fixture("eve.pgp")], &[]).unwrap();
    assert_eq!(signature.check, Check::KeyChanged { signer: "ada@example.org".into() });
}
