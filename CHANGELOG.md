# Changelog

What changed in each version of BeaverMail, newest first.

## 0.2.0 – 2026-11-11

### Added

- A right-click menu on the list's messages, acting on the picked ones or on its own row without opening it: open in a tab or in its folder, answer, mark, pin, star, label, move, archive, trash, Spam or Not spam, trust or block the sender, view the source.
- The message list ordered per folder, by date, sender or subject either way, pinned first; oldest first opens at the bottom.
- The list as cards or as a table of From, Subject and Date, with columns widened by dragging their edges; the message shown beside the list, below it or only in a tab.
- Appearance in Settings, with the density of the sidebars: Compact, Normal or Comfortable.
- Settings in a window that can be moved and resized, its place and size kept, closed with Esc or its X.
- A diagnostic log, turned on in Settings › Troubleshooting, that never holds the text, subject or addresses of a message.
- The text of a message selected from end to end: dragged up or down, kept while scrolling and extended with Shift+click; a word with a double click, a paragraph with three; Copy and Select all on a right click.
- The source of a message selected the same way, from beside a line too, and all of it with Ctrl+A.
- Search in the folder shown, its account or everywhere, picked from labels under the search box instead of typing in:all; a search that finds nothing offers to look everywhere. Searching every folder leaves out Spam and Trash; in:all looks there too. Typing in: lists the folders to pick from, Other folder… among the labels searches any one, in:sent, in:spam and the other usual folders find each account's whatever its server calls it, and in:testing finds a folder inside another by its own name.
- The data folder chosen in Settings › Stored mail, on another disk too: BeaverMail checks the room, starts again and moves everything there, going on after an interruption.
- Search on the server, beside the results or when nothing is found, for mail older than what is kept on this computer or never downloaded: folder by folder with its results shown as they come, stopped at once with Stop; what it finds is kept and opens like the rest.
- Archive into any folder of the account, chosen in its settings, and optionally into a folder for each year inside it, made when first needed.
- Search options beside the search box: from, to, subject, words, dates, attachment and unread as fields, written into the box as the words they stand for, and read back from it.
- Filters in Settings › Filters: rules on the sender, recipients, subject or attachments that move, star, label, mark as read or trash new mail in the Inbox as it arrives, in order, each stopping the rest when asked; Run now applies one to the mail already there, and Create filter… on a message starts one from its sender.
- Labels in Settings › Labels: the five standard ones renamed and recoloured, labels of your own made and deleted, names and colours kept on this computer; every label in the Label menu and in filters, 0 to take them all off, label:work in a search, and a Labels section in the sidebar showing each one's mail everywhere.
- Calendars in Settings: each server with its calendars, renamed, recoloured, made default or removed, its password changed once the server takes it, or all of it removed.
- Spell check while writing: words spelled wrong underlined, their spellings and Add to dictionary on a right click, the language chosen in Settings › Writing or per message; dictionaries from the system or downloaded on request; Check spelling (F7) going through the words marked one by one, to change, ignore or add each, optionally before sending.

### Changed

- On servers that keep mod-sequences (CONDSTORE: Gmail, Dovecot, Fastmail and others), a folder that did not change since the last sync is not asked again, and one where only flags changed is asked only for those, across the whole folder. Where the server offers QRESYNC too (Dovecot, Fastmail, Cyrus and others), it also names the messages deleted since, so deletions are known across the whole folder without comparing lists.
- Accounts in Settings › Accounts, a list that opens each account's page, instead of under the other pages.
- Settings › Encryption is now Secure mail, set apart from Stored mail, where mail on this computer is encrypted.
- Fewer false alarms in the spam checks.
- All inboxes, Suspicious and Spam get the menus of an account's own folders.
- Folders start folded in the sidebar, and those opened are remembered; a folder opened from anywhere shows in the sidebar.
- Settings chosen from drop-downs, each saying what its choice does.
- Scroll bars shown at rest wherever there is more to scroll.

### Fixed

- On an account whose server has no Drafts, Sent or Trash folder yet, the first draft, sent copy or deletion makes it, instead of failing; a folder the server says is missing when mail is saved or moved into it (NO [TRYCREATE]) is made and the mail goes in.
- Labels show in the table layout of the list too, as in the cards.
- A folder keeps syncing when the server describes one of its messages in a way that cannot be read, instead of the account showing as offline.
- With no encryption set, a server that signs in only over an encrypted connection gets no password, and says to choose STARTTLS or SSL/TLS instead of "sign-in refused".
- Messages open in a twentieth of a second: over a connection of their own, without waiting for the background download or its writes to the archive, and without checking again what was checked when they arrived.
- The window stays responsive while mail downloads: the list and the open message are no longer worked out again at every frame.
- Closing a message tab leaves the list where it was, instead of at the top.
- An account no longer shows as offline after its server ends a quiet session.
- A message deleted or moved no longer comes back in a list opened before the server has made the change.
- The unread counts follow a message read or moved from a search result or a tab, and are asked of the server again after each run of changes.
- A folder's message count drops at once when a message is deleted or moved, and comes back with Undo.
- Adding calendars or address books offers the login of any mail account, not only the first one's.
- A search word with an accented letter where in: would be no longer closes the app.
- A new event's Calendar list shows every calendar, by server and named with it, the read-only ones dimmed with the reason.

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
