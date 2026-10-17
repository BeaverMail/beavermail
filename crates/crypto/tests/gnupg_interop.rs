// SPDX-License-Identifier: AGPL-3.0-only

//! Writes a key, its backup and a sealed message for checking by hand with GnuPG; run with
//! `INTEROP_DIR=<dir> cargo test -p beavermail-crypto --test gnupg_interop -- --ignored`.

use beavermail_crypto::{encrypt, export, generate, public_key};

#[test]
#[ignore]
fn write_files_for_gnupg() {
    let dir = std::env::var("INTEROP_DIR").expect("INTEROP_DIR");
    let ada = generate("Ada", "ada@example.org").unwrap();
    std::fs::write(format!("{dir}/ada.key"), &ada).unwrap();
    std::fs::write(format!("{dir}/ada-backup.asc"), export(&ada, "1111-2222-3333").unwrap()).unwrap();
    let message = b"From: Ada <ada@example.org>\r\nTo: Ada <ada@example.org>\r\nSubject: hi\r\nContent-Type: text/plain\r\n\r\nSealed by BeaverMail.\r\n";
    std::fs::write(format!("{dir}/sealed.eml"), encrypt(message, &ada, &[public_key(&ada).unwrap()], &[]).unwrap()).unwrap();
}
