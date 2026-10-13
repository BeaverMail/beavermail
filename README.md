# BeaverMail client

Desktop mail client written in Rust with [egui](https://github.com/emilk/egui) (eframe).
Licensed under AGPL-3.0-only (see [LICENSE](LICENSE)).

## Layout

- `crates/core`: domain model and the `MailSource` trait. No GUI or network dependencies.
- `crates/imap`: IMAP backend (`ImapSource`).
- `crates/accounts`: `accounts.toml` and the system credential store.
- `crates/app`: the egui desktop shell (binary `beavermail`).

## Status

Several IMAP accounts with folders, message list and reader. Messages can be marked read or unread,
archived, moved to Trash and erased from Trash (keys: Up/Down, E, Delete, U). New messages, replies,
reply all and forwards are sent over SMTP (keys: C, R, A, F), with a copy kept in Sent. Accounts can
be edited from the sidebar. No search beyond the loaded list, no local cache, HTML mail or encryption yet.

## Running

```sh
cargo run -p beavermail
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
