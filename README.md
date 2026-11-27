<p align="center"><img src="crates/app/assets/beavermail.svg" alt="BeaverMail" width="180"></p>

# BeaverMail

Mail, contacts and calendar in one desktop app, private by default: the archive is encrypted on your
computer, remote content stays blocked, and it works offline. Written in Rust with
[egui](https://github.com/emilk/egui). Licensed under AGPL-3.0-only (see [LICENSE](LICENSE)).

## Status

- Accounts: any IMAP/SMTP server (Gmail with an app password); several at once, with All inboxes and every account's Drafts, Sent, Archive, Spam and Trash together; mail moved between accounts; server storage shown, with a warning when nearly full; folders shown as subscribed on the server, chosen in Folders…
- Offline: encrypted local archive with full-text search (encrypted mail included, once opened with your key) and saved searches in the sidebar; changes and mail wait in an Outbox for the server; folders on this computer (Inbox, Archive, Trash and your own) to move mail off the servers and back.
- Reading: plain text, simplified or original HTML; conversations, tabs, nothing remote loaded unless allowed; the list as cards or a table, ordered per folder, the message beside it, below it or in a tab.
- Safety: sender checks, look-alike domains and links, risky attachments; held "Suspicious" mail and a spam classifier.
- Encryption: OpenPGP (with Autocrypt) and S/MIME, signed and encrypted mail.
- Writing: Markdown, address chips, drafts kept locally and on the server, undo send, send later, signatures, other addresses (aliases) with their own name and signature, spell check.
- Undo: Archive and Move to Trash wait 10 s before reaching the server.
- Filters: rules that move, star, label or mark new Inbox mail as it arrives.
- Assistant: replies drafted and conversations summarised by a language model with your own key (Anthropic, OpenAI or local), asked first and recorded.
- Contacts: cards, an address book on this computer and address books synced over CardDAV; import from vCard or CSV (Google, Outlook, Thunderbird).
- Calendar: week, month and agenda over CalDAV or on this computer; events made and changed here; invitations answered from mail; calendars renamed, recoloured and their servers' passwords changed in Settings.
- Attachments: every file of the downloaded mail, encrypted mail included, found by name, sender or kind, as a list or a grid of pictures.
- Tasks: lists over CalDAV (made here when a server has none), grouped by due date and put in order by dragging, with subtasks, labels and priority; changes kept offline and merged with those made elsewhere.

Keys (Ctrl+K lists every command): Up/Down, Page Up/Down, Home/End (with Shift, pick a range), F6, Enter,
E, Delete, V (move to folder), U, P, S, T (make a task), 1–5, 0, `!`, C or Ctrl+N, R, A, F, `/`, Ctrl+Z, Ctrl+U, Ctrl+W, Ctrl+Tab, Ctrl+, (Settings); in the calendar
T, Left/Right, W, M, A, N; in Tasks Up/Down, Space, Delete, N or `/`, Esc; in Attachments the arrows and Enter.

Drag and drop of attachments does not work on Wayland yet: start with `WAYLAND_DISPLAY=` to use X11.

## Layout

- `crates/core`: domain model and the `MailSource` trait, no GUI or network.
- `crates/store`: the local mail index (SQLCipher, full-text search).
- `crates/vault`: the encrypted message files and their keys.
- `crates/imap`: IMAP backend (`ImapSource`), keeping the archive in sync.
- `crates/smtp`: sending (`SmtpSender`).
- `crates/crypto`: S/MIME and OpenPGP.
- `crates/carddav`: address books (CardDAV), calendars and tasks (CalDAV).
- `crates/accounts`: `accounts.toml` and the system credential store.
- `crates/app`: the egui desktop app (binary `beavermail`).

## Building

```sh
cargo run -p beavermail
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo run -p beavermail -- --screen-dry-run` prints what the spam checks would do to each Inbox, without moving anything.
