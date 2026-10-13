# BeaverMail client

Desktop mail client written in Rust with [egui](https://github.com/emilk/egui) (eframe).
Licensed under AGPL-3.0-only (see [LICENSE](LICENSE)).

## Layout

- `crates/core`: domain model and the `MailSource` trait. No GUI or network dependencies.
- `crates/store`: the local mail index (SQLite, full-text search); it holds no message bodies.
- `crates/vault`: the message files, their encryption and key slots.
- `crates/imap`: IMAP backend (`ImapSource`), which keeps the archive in sync.
- `crates/smtp`: sending (`SmtpSender`).
- `crates/accounts`: `accounts.toml` and the system credential store.
- `crates/app`: the egui desktop shell (binary `beavermail`).

## Status

Several IMAP accounts with folders, message list and reader. Messages can be marked read or unread,
archived, moved to Trash and erased from Trash (keys: Up/Down, E, Delete, U). New messages, replies,
reply all and forwards are sent over SMTP (keys: C, R, A, F), with a copy kept in Sent. Accounts can
be edited from the sidebar. Messages open as plain text by default, or as simplified HTML (Settings, or per message and per
sender); images and styling are never loaded and tracking parameters are removed from links.
Mail is kept locally: the window opens from a SQLite index, and each downloaded message is an
file under the data directory, encrypted by default (XChaCha20-Poly1305). Settings choose between a key
kept by the system, a passphrase, or no encryption.
New mail, flags, deletions and older history sync in the background, and the messages themselves
download in the background (per account: everything, last 12 months, last 30 days, or only opened
messages), with progress and a pause button in the status bar. The index itself is not
encrypted yet. Attachments are listed in the reader and can be opened or saved to Downloads (programs and macro
documents ask first); no attaching files to new messages yet, original HTML or end-to-end
message encryption yet.

## Running

```sh
cargo run -p beavermail
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
