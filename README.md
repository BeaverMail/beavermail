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

- Several IMAP accounts, with All inboxes (each account's mail marked by its colour); send, reply, forward over SMTP with a copy in Sent.
- Local archive: SQLCipher index plus encrypted message files; background sync, IDLE, full-text search; works offline (read, search, draft; sending and changes wait in an Outbox until the server answers); "Work offline" in the status bar stops all network use.
- Archive and Move to Trash wait 10 s before reaching the server: Undo (Ctrl+Z) takes them back.
- Reader: plain text, simplified HTML or original HTML (Blitz); nothing remote loads unless allowed, and tracking pixels never do (the bar lists them).
- Sender checks (server authentication results, look-alike names and domains, misleading links, risky attachments).
- Spam: mail from new senders that fails a check is held as "Suspicious"; a classifier learns from what you mark.
- Unsubscribe from mailing lists (one-click for verified senders, else an email or the sender's page) and block a sender.
- Markdown compose, drafts kept locally and on the server, attachments; undo send (configurable), signatures per account.
- Conversations: one row per thread (References, In-Reply-To, "Re:" subjects) with your replies from Sent; can be turned off.
- Messages open in tabs (double click); the reading pane beside the list can be turned off, so a message opens in the list's place.

Keys (Ctrl+K lists every command with its key, and goes to any folder): Up/Down, Page Up/Down, Home/End, F6 (list or reader; in the reader the same keys and Space scroll), Enter, E, Delete, U, `!` (spam), C or Ctrl+N (new), R, A, F, `/`, Ctrl+W, Ctrl+Tab; in a new message Esc or Ctrl+Shift+D discards.

`cargo run -p beavermail -- --screen-dry-run` prints what the spam checks would do to each Inbox, without moving anything.

Drag and drop of attachments does not work on Wayland yet: start with `WAYLAND_DISPLAY=` to use X11.

## Running

```sh
cargo run -p beavermail
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
