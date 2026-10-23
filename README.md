# BeaverMail client

Desktop mail client written in Rust with [egui](https://github.com/emilk/egui) (eframe).
Licensed under AGPL-3.0-only (see [LICENSE](LICENSE)).

## Layout

- `crates/core`: domain model and the `MailSource` trait. No GUI or network dependencies.
- `crates/store`: the local mail index (SQLite, full-text search); it holds no message bodies.
- `crates/vault`: the message files, their encryption and key slots.
- `crates/imap`: IMAP backend (`ImapSource`), which keeps the archive in sync.
- `crates/smtp`: sending (`SmtpSender`).
- `crates/crypto`: signed and encrypted mail (S/MIME and OpenPGP).
- `crates/carddav`: address books on a CardDAV server.
- `crates/accounts`: `accounts.toml` and the system credential store.
- `crates/app`: the egui desktop shell (binary `beavermail`).

## Status

- Sign in with Google or Microsoft (OAuth2, no password stored), or any IMAP account.
- Several IMAP accounts, with All inboxes (each account's mail marked by its colour); send, reply, forward over SMTP with a copy in Sent.
- Local archive: SQLCipher index plus encrypted message files; background sync, IDLE, full-text search; works offline (read, search, draft; sending and changes wait in an Outbox until the server answers); "Work offline" in the status bar stops all network use.
- Archive and Move to Trash wait 10 s before reaching the server: Undo (Ctrl+Z) takes them back.
- Reader: plain text, simplified HTML or original HTML (Blitz); nothing remote loads unless allowed, and tracking pixels never do (the bar lists them); links open and copy without redirects or tracking parameters.
- Sender checks (server authentication results, look-alike names and domains, misleading links, risky attachments); S/MIME and OpenPGP signatures verified.
- Encryption: an OpenPGP key per account in one click (or imported, or an S/MIME certificate), handed out by Autocrypt; encrypted mail opens in the reader and goes out encrypted when every recipient's key is known.
- Spam: mail from new senders that fails a check is held as "Suspicious"; a classifier learns from what you mark; trust or block a sender, and review both lists in Settings.
- Unsubscribe from mailing lists (one-click for verified senders, else an email or the sender's page) and block a sender.
- Contact cards (click a sender): name, notes, your latest conversations; saved contacts in their own section, from the rail at the window's left edge, synced with CardDAV address books (Fastmail, Nextcloud, iCloud…) added there, each shown on its own or all together; every field and the photo edited; only contacts you add to one go to the server.
- Markdown compose, addresses as chips (a wrong one in red) with suggestions from everyone you wrote to, Cc and Bcc on demand (Bcc kept in your Sent copy only), drafts kept locally and on the server, attachments; undo send (configurable), send later, signatures per account.
- Conversations: one row per thread (References, In-Reply-To, "Re:" subjects) with your replies from Sent, read in a row with several messages open and quoted text folded; can be turned off.
- Messages open in tabs (double click); the reading pane beside the list can be turned off, and then every message opens in a tab.
- Calendar, from the rail: CalDAV calendars and one on this computer, in week, month and agenda views, with repeating events and time zones; keys T (today), Left/Right, W, M, A. New event (N, or a click on an empty hour of the week), and the user's own events changed or deleted from their details. Invitations in mail: Accept, Maybe or Decline, with what they clash with; cancellations and events to add.

Keys (Ctrl+K lists every command with its key, and goes to any folder): Up/Down, Page Up/Down, Home/End, F6 (list or reader; in the reader the same keys and Space scroll), Enter, E, Delete, U, P (pin), S (star), 1–5 (labels), `!` (spam), C or Ctrl+N (new), R, A, F, `/`, Ctrl+U (source, Ctrl+F finds in it), Ctrl+W, Ctrl+Tab; in a new message Esc or Ctrl+Shift+D discards.

`cargo run -p beavermail -- --screen-dry-run` prints what the spam checks would do to each Inbox, without moving anything.

Drag and drop of attachments does not work on Wayland yet: start with `WAYLAND_DISPLAY=` to use X11.

## Running

```sh
cargo run -p beavermail
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

"Sign in with Google/Microsoft" shows only when the build has the app's client IDs: set `BEAVERMAIL_GOOGLE_CLIENT_ID`, `BEAVERMAIL_GOOGLE_CLIENT_SECRET` and `BEAVERMAIL_MICROSOFT_CLIENT_ID` when building.

Builds for Windows (standalone .exe and installer), macOS (BeaverMail.app, not notarised) and Linux (.tar.gz and .deb) come from the workflows in `.github/workflows`, run by hand or by a `v*` tag.
