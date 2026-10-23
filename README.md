<p align="center"><img src="crates/app/assets/beavermail.svg" alt="BeaverMail" width="180"></p>

# BeaverMail

Mail, contacts and calendar in one desktop app, private by default: the archive is encrypted on your
computer, remote content stays blocked, and it works offline. Written in Rust with
[egui](https://github.com/emilk/egui). Licensed under AGPL-3.0-only (see [LICENSE](LICENSE)).

## Status

- Accounts: any IMAP/SMTP server, or Google and Microsoft sign-in (OAuth2); several at once, with All inboxes.
- Offline: encrypted local archive with full-text search; changes and mail wait in an Outbox for the server.
- Reading: plain text, simplified or original HTML; conversations, tabs, nothing remote loaded unless allowed.
- Safety: sender checks, look-alike domains and links, risky attachments; held "Suspicious" mail and a spam classifier.
- Encryption: OpenPGP (with Autocrypt) and S/MIME, signed and encrypted mail.
- Writing: Markdown, address chips, drafts kept locally and on the server, undo send, send later, signatures.
- Undo: Archive and Move to Trash wait 10 s before reaching the server.
- Contacts: cards, address books synced over CardDAV.
- Calendar: week, month and agenda over CalDAV or on this computer; events made and changed here; invitations answered from mail.

Keys (Ctrl+K lists every command): Up/Down, Page Up/Down, Home/End (with Shift, pick a range), F6, Enter,
E, Delete, V (move to folder), U, P, S, 1–5, `!`, C or Ctrl+N, R, A, F, `/`, Ctrl+Z, Ctrl+U, Ctrl+W, Ctrl+Tab; in the calendar
T, Left/Right, W, M, A, N.

Drag and drop of attachments does not work on Wayland yet: start with `WAYLAND_DISPLAY=` to use X11.

## Layout

- `crates/core`: domain model and the `MailSource` trait, no GUI or network.
- `crates/store`: the local mail index (SQLCipher, full-text search).
- `crates/vault`: the encrypted message files and their keys.
- `crates/imap`: IMAP backend (`ImapSource`), keeping the archive in sync.
- `crates/smtp`: sending (`SmtpSender`).
- `crates/crypto`: S/MIME and OpenPGP.
- `crates/carddav`: address books (CardDAV) and calendars (CalDAV).
- `crates/accounts`: `accounts.toml` and the system credential store.
- `crates/app`: the egui desktop app (binary `beavermail`).

## Building

```sh
cargo run -p beavermail
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo run -p beavermail -- --screen-dry-run` prints what the spam checks would do to each Inbox, without moving anything.
