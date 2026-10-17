// SPDX-License-Identifier: AGPL-3.0-only

//! The user's own keys: made, exported, imported; and messages sealed and opened with them.

use beavermail_crypto::{Check, ImportError, Opened, add_autocrypt, autocrypt_key, backup_password, decrypt, encrypt, export, generate, import, public_key, short_id};

const MESSAGE: &[u8] = b"From: Ada Lovelace <ada@example.org>\r\nTo: Bob <bob@example.org>\r\nSubject: Lunch\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n\r\nSee you at noon.\r\n";

#[test]
fn a_sealed_message_opens_for_its_recipient_with_the_senders_signature() {
    let (ada, bob, eve) = (generate("Ada", "ada@example.org").unwrap(), generate("Bob", "bob@example.org").unwrap(), generate("Eve", "eve@example.org").unwrap());
    let sealed = encrypt(MESSAGE, &ada, &[public_key(&bob).unwrap(), public_key(&ada).unwrap()], &[]).unwrap();
    let text = String::from_utf8_lossy(&sealed);
    assert!(text.contains("multipart/encrypted") && text.contains("Subject: Lunch") && !text.contains("See you at noon"), "{text}");
    match decrypt(&sealed, std::slice::from_ref(&bob), &[public_key(&ada).unwrap()]).unwrap() {
        Opened::Message(raw, signature) => {
            let raw = String::from_utf8_lossy(&raw);
            assert!(raw.contains("Subject: Lunch") && raw.contains("Content-Type: text/plain") && raw.contains("See you at noon."), "{raw}");
            assert!(matches!(signature.unwrap().check, Check::Valid { signer, .. } if signer == "ada@example.org"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(decrypt(&sealed, &[ada], &[]).unwrap(), Opened::Message(..)), "the sender's own copy opens too");
    assert!(matches!(decrypt(&sealed, &[eve], &[]).unwrap(), Opened::NoKey));
    assert!(decrypt(MESSAGE, &[bob], &[]).is_none(), "not encrypted");
}

#[test]
fn a_blind_copy_recipient_can_open_it_unnamed() {
    let (ada, bob, carol) = (generate("Ada", "ada@example.org").unwrap(), generate("Bob", "bob@example.org").unwrap(), generate("Carol", "carol@example.org").unwrap());
    let sealed = encrypt(MESSAGE, &ada, &[public_key(&bob).unwrap()], &[public_key(&carol).unwrap()]).unwrap();
    assert!(matches!(decrypt(&sealed, &[carol], &[]).unwrap(), Opened::Message(..)));
}

#[test]
fn a_key_leaves_only_under_a_password_and_comes_back_with_it() {
    let key = generate("Ada", "ada@example.org").unwrap();
    let password = backup_password();
    assert_eq!(password.len(), 9 * 4 + 8);
    assert!(password.split('-').all(|g| g.len() == 4 && g.chars().all(|c| c.is_ascii_digit())));
    let backup = export(&key, &password).unwrap();
    assert!(backup.starts_with("-----BEGIN PGP PRIVATE KEY BLOCK-----"));
    assert_eq!(import(backup.as_bytes(), None), Err(ImportError::NeedsPassword));
    assert_eq!(import(backup.as_bytes(), Some("1234")), Err(ImportError::WrongPassword));
    let back = import(backup.as_bytes(), Some(&password)).unwrap();
    assert_eq!(short_id(&back), short_id(&key));
    assert_eq!(import(&public_key(&key).unwrap(), None), Err(ImportError::NotASecretKey), "a public key is not one to import");
}

#[test]
fn the_key_travels_in_the_autocrypt_header() {
    let key = generate("Ada", "ada@example.org").unwrap();
    let public = public_key(&key).unwrap();
    let raw = add_autocrypt(MESSAGE, "ada@example.org", &public);
    assert_eq!(autocrypt_key(&raw), Some(("ada@example.org".to_owned(), public)));
    let id = short_id(&key).unwrap();
    assert_eq!(id.len(), 19, "{id}");
}

#[test]
fn a_message_sealed_by_gnupg_opens() {
    let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let (raw, key) = (std::fs::read(format!("{dir}/gnupg-sealed.eml")).unwrap(), std::fs::read(format!("{dir}/own-ada.key")).unwrap());
    match decrypt(&raw, &[key], &[]).unwrap() {
        Opened::Message(raw, signature) => {
            assert!(String::from_utf8_lossy(&raw).contains("Sealed by GnuPG."));
            assert!(signature.is_none(), "GnuPG was not asked to sign");
        }
        other => panic!("{other:?}"),
    }
}
