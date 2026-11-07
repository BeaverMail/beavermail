# Changelog

What changed in each version of BeaverMail, newest first.

## Unreleased

### Added

- A right-click menu on the list's messages, acting on the picked ones or on its own row without opening it: open in a tab or in its folder, answer, mark, pin, star, label, move, archive, trash, Spam or Not spam, trust or block the sender, view the source.
- The message list ordered per folder, by date, sender or subject either way, pinned first; oldest first opens at the bottom.
- The list as cards or as a table of From, Subject and Date, with columns widened by dragging their edges; the message shown beside the list, below it or only in a tab.
- Appearance in Settings, with the density of the sidebars: Compact, Normal or Comfortable.
- A diagnostic log, turned on in Settings › Troubleshooting, that never holds the text, subject or addresses of a message.
- The text of a message selected from end to end: dragged up or down, kept while scrolling and extended with Shift+click; a word with a double click, a paragraph with three; Copy and Select all on a right click.
- The source of a message selected the same way, from beside a line too, and all of it with Ctrl+A.
- Spell check while writing: words spelled wrong underlined, their spellings and Add to dictionary on a right click, the language chosen in Settings › Writing or per message; dictionaries from the system or downloaded on request; Check spelling (F7) going through the words marked one by one, to change, ignore or add each, optionally before sending.

### Changed

- Fewer false alarms in the spam checks.
- All inboxes, Suspicious and Spam get the menus of an account's own folders.
- Folders start folded in the sidebar, and those opened are remembered; a folder opened from anywhere shows in the sidebar.
- Settings chosen from drop-downs, each saying what its choice does.
- Scroll bars shown at rest wherever there is more to scroll.

### Fixed

- An account no longer shows as offline after its server ends a quiet session.
- A message deleted or moved no longer comes back in a list opened before the server has made the change.
- The unread counts follow a message read or moved from a search result or a tab, and are asked of the server again after each run of changes.

## 0.1.1 – 2026-10-23

### Added

- Move messages to any folder of their account: V, the command palette, the folder button in the reader, or by dragging them onto a folder. Undo works as for Archive.
- New folder, Rename and Delete folder from a folder's right-click menu, and New folder from the command palette.
- Deleting a folder moves it into Trash, with the folders inside it and their messages, from where it can be brought back. Only a folder already in Trash is deleted for good.
- Folders inside others are shown under them, and can be folded; a folded one counts the unread mail of those inside it.

### Changed

- Empty Trash also deletes the folders moved into it.
- Folder names with accented letters show as written.
- Menus list only what can be done in each folder, with more room around their text.

## 0.1.0 – 2026-10-23

First release.

- Mail over IMAP and SMTP, or with Google and Microsoft sign-in; several accounts and All inboxes.
- An encrypted archive on this computer, with full-text search, that works offline.
- Sender checks, held suspicious mail and a spam classifier; OpenPGP and S/MIME.
- Contacts over CardDAV and a calendar over CalDAV, with invitations answered from mail.
- Packages for Windows (installer and standalone), macOS and Linux (.deb and .tar.gz).
