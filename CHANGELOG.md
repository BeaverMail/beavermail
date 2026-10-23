# Changelog

What changed in each version of BeaverMail, newest first.

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
