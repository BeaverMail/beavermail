// SPDX-License-Identifier: AGPL-3.0-only

//! The IMAP backend against a scripted local server, so writes (flags, moves, erase) can be
//! checked without touching a real mailbox. The server records every command it receives.

use std::sync::{Arc, Mutex};

use beavermail_core::{Account, MailError, MailSource, MessageId, Role, Server, Tls};
use beavermail_imap::ImapSource;
use beavermail_store::Store;
use beavermail_vault::Vault;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const HEADER: &str = "From: Ada <ada@example.com>\r\nSubject: Hello\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\n\r\n";

/// The one message the server holds, complete.
const MESSAGE: &str = "From: Ada <ada@example.com>\r\nTo: Me <me@example.com>, grace@example.com\r\nCc: Linus <linus@example.com>\r\nReply-To: lists@example.com\r\nSubject: Hello\r\nMessage-ID: <m2@example.com>\r\nReferences: <m0@example.com> <m1@example.com>\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\n\r\nHi there.\r\n";

struct Mock {
    source: ImapSource,
    account: Account,
    store: Arc<Store>,
    files: Arc<Vault>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    /// Commands received so far, after the tag.
    fn commands(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Commands that change messages, in order.
    fn writes(&self) -> Vec<String> {
        let writes = ["UID STORE", "UID MOVE", "UID COPY", "UID EXPUNGE"];
        self.commands().into_iter().filter(|c| writes.iter().any(|w| c.starts_with(w))).collect()
    }
}

/// `move_works = false` makes `UID MOVE` fail, as on a server without the MOVE extension.
async fn start(move_works: bool) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let served = log.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, served.clone(), move_works));
        }
    });
    let server = Server { host: "127.0.0.1".into(), port, tls: Tls::None };
    let account = Account { name: "mock".into(), user: "u".into(), email: String::new(), imap: server.clone(), smtp: server, save_sent: true };
    let store = Arc::new(Store::in_memory().unwrap());
    let files = Arc::new(Vault::in_memory());
    Mock { source: ImapSource::new(&account, "p".into(), store.clone(), files.clone()), account, store, files, log }
}

async fn serve(socket: TcpStream, log: Arc<Mutex<Vec<String>>>, move_works: bool) {
    let (read, mut write) = socket.into_split();
    let mut read = BufReader::new(read);
    let _ = write.write_all(b"* OK ready\r\n").await;
    let mut line = String::new();
    loop {
        line.clear();
        if !matches!(read.read_line(&mut line).await, Ok(n) if n > 0) {
            return;
        }
        let line = line.trim_end().to_owned();
        let Some((tag, command)) = line.split_once(' ') else { continue };
        log.lock().unwrap().push(command.to_owned());
        let verb = command.to_ascii_uppercase();
        let reply = if verb.starts_with("LIST") {
            format!(
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\Archive) \"/\" \"Archive\"\r\n* LIST (\\Sent) \"/\" \"Sent\"\r\n* LIST (\\Trash) \"/\" \"Trash\"\r\n{tag} OK done\r\n"
            )
        } else if verb.starts_with("STATUS") {
            let name = command.split(' ').nth(1).unwrap_or("\"INBOX\"");
            format!("* STATUS {name} (UNSEEN 1)\r\n{tag} OK done\r\n")
        } else if verb.starts_with("SELECT") || verb.starts_with("EXAMINE") {
            format!("* 1 EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 1] ok\r\n{tag} OK [READ-WRITE] done\r\n")
        } else if verb.contains("BODY.PEEK[]") {
            let n = MESSAGE.len();
            format!("* 1 FETCH (UID 5 BODY[] {{{n}}}\r\n{MESSAGE})\r\n{tag} OK done\r\n")
        } else if verb.contains("FETCH") && verb.contains("HEADER") {
            let n = HEADER.len();
            format!("* 1 FETCH (UID 5 FLAGS () BODY[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES)] {{{n}}}\r\n{HEADER})\r\n{tag} OK done\r\n")
        } else if verb.contains("FETCH") {
            format!("* 1 FETCH (UID 5 FLAGS ())\r\n{tag} OK done\r\n")
        } else if verb.starts_with("APPEND") {
            // "APPEND "Sent" (\Seen) {123}": ask for the literal, read it, and record what arrived.
            let size: usize = command.rsplit('{').next().and_then(|n| n.trim_end_matches('}').parse().ok()).unwrap_or(0);
            if write.write_all(b"+ go ahead\r\n").await.is_err() {
                return;
            }
            let mut data = vec![0; size];
            if read.read_exact(&mut data).await.is_err() {
                return;
            }
            let mut rest = String::new();
            let _ = read.read_line(&mut rest).await; // the CRLF that ends the command
            log.lock().unwrap().push(format!("APPEND-DATA {}", String::from_utf8_lossy(&data)));
            format!("{tag} OK [APPENDUID 1 1] done\r\n")
        } else if verb.starts_with("UID MOVE") && !move_works {
            format!("{tag} NO [CANNOT] MOVE is not supported\r\n")
        } else if verb.starts_with("UID EXPUNGE") {
            format!("* 1 EXPUNGE\r\n{tag} OK done\r\n")
        } else {
            format!("{tag} OK done\r\n")
        };
        if write.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// The id of the one message the mock holds in `mailbox`.
async fn message_in(mock: &Mock, mailbox: &str) -> MessageId {
    mock.source.mailboxes().await.expect("list folders");
    mock.source.sync(mailbox).await.expect("sync");
    mock.source.messages(mailbox).await.expect("list messages").first().expect("one message").id
}

#[tokio::test]
async fn folders_get_their_roles() {
    let mock = start(true).await;
    let mailboxes = mock.source.mailboxes().await.unwrap();
    let role = |name: &str| mailboxes.iter().find(|m| m.name == name).unwrap().role;
    assert_eq!(mailboxes[0].name, "INBOX");
    assert_eq!(role("INBOX"), Role::Inbox);
    assert_eq!(role("Archive"), Role::Archive);
    assert_eq!(role("Trash"), Role::Trash);
}

#[tokio::test]
async fn reading_flags_are_stored() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    mock.source.set_unread(id, false).await.unwrap();
    mock.source.set_unread(id, true).await.unwrap();
    assert_eq!(mock.writes(), ["UID STORE 5 +FLAGS.SILENT (\\Seen)", "UID STORE 5 -FLAGS.SILENT (\\Seen)"]);
}

#[tokio::test]
async fn archive_and_trash_move_to_their_folders() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    mock.source.archive(id).await.unwrap();
    // The archive forgot the message at once; a sync brings it back (the server still lists it).
    assert!(mock.source.messages("INBOX").await.unwrap().is_empty());
    let id = message_in(&mock, "INBOX").await;
    mock.source.trash(id).await.unwrap();
    assert_eq!(mock.writes(), ["UID MOVE 5 \"Archive\"", "UID MOVE 5 \"Trash\""]);
}

#[tokio::test]
async fn move_falls_back_to_copy_and_expunge() {
    let mock = start(false).await;
    let id = message_in(&mock, "INBOX").await;
    mock.source.trash(id).await.unwrap();
    assert_eq!(
        mock.writes(),
        ["UID MOVE 5 \"Trash\"", "UID COPY 5 \"Trash\"", "UID STORE 5 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 5"]
    );
}

#[tokio::test]
async fn erase_is_refused_outside_the_trash() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    let error = mock.source.erase(id).await.unwrap_err();
    assert!(matches!(error, MailError::Backend(_)), "{error:?}");
    assert!(mock.writes().is_empty(), "nothing may reach the server: {:?}", mock.writes());
}

#[tokio::test]
async fn erase_in_the_trash_deletes_for_good() {
    let mock = start(true).await;
    let id = message_in(&mock, "Trash").await;
    mock.source.erase(id).await.unwrap();
    assert_eq!(mock.writes(), ["UID STORE 5 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 5"]);
}

#[tokio::test]
async fn moving_within_the_target_folder_is_refused() {
    let mock = start(true).await;
    let id = message_in(&mock, "Trash").await;
    assert!(mock.source.trash(id).await.is_err());
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn opening_a_message_reads_recipients_and_threading_headers() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    let summary = &mock.source.messages("INBOX").await.unwrap()[0];
    assert_eq!(summary.address, "ada@example.com");
    let body = mock.source.body(id).await.unwrap();
    assert_eq!(body.text.trim(), "Hi there.");
    assert_eq!(body.to, ["Me <me@example.com>", "grace@example.com"]);
    assert_eq!(body.cc, ["Linus <linus@example.com>"]);
    assert_eq!(body.reply_to, ["lists@example.com"]);
    assert_eq!(body.message_id, "m2@example.com");
    assert_eq!(body.references, ["m0@example.com", "m1@example.com"]);
}

#[tokio::test]
async fn a_sent_copy_is_appended_to_sent_as_read() {
    let mock = start(true).await;
    let raw = b"Subject: Sent one\r\n\r\nBody\r\n".to_vec();
    mock.source.save_sent(raw).await.unwrap();
    let commands = mock.commands();
    assert!(commands.iter().any(|c| c.starts_with("APPEND \"Sent\" (\\Seen)")), "{commands:?}");
    assert!(commands.iter().any(|c| c.starts_with("APPEND-DATA Subject: Sent one")), "{commands:?}");
}

#[tokio::test]
async fn syncing_twice_does_not_duplicate_and_reports_no_more_history() {
    let mock = start(true).await;
    mock.source.mailboxes().await.unwrap();
    let first = mock.source.sync("INBOX").await.unwrap();
    mock.source.sync("INBOX").await.unwrap();
    assert_eq!(mock.source.messages("INBOX").await.unwrap().len(), 1);
    assert_eq!(first.remaining, 0, "a one-message folder has no older history");
}

#[tokio::test]
async fn the_archive_works_without_the_server() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    let body = mock.source.body(id).await.unwrap();
    // A second source over the same archive whose server is unreachable (nothing listens on port 1).
    let mut account = mock.account.clone();
    account.imap.port = 1;
    let offline = ImapSource::new(&account, "p".into(), mock.store.clone(), mock.files.clone());
    assert_eq!(offline.cached_mailboxes().await.unwrap().len(), 4);
    assert_eq!(offline.messages("INBOX").await.unwrap().len(), 1);
    assert_eq!(offline.body(id).await.unwrap(), body, "an opened message is readable offline");
    assert!(offline.sync("INBOX").await.is_err());
    assert_eq!(offline.messages("INBOX").await.unwrap().len(), 1, "a failed sync loses nothing");
}

#[tokio::test]
async fn a_message_keeps_its_id_across_syncs() {
    let mock = start(true).await;
    let first = message_in(&mock, "INBOX").await;
    mock.source.sync("INBOX").await.unwrap();
    assert_eq!(mock.source.messages("INBOX").await.unwrap()[0].id, first);
}

#[tokio::test]
async fn a_downloaded_message_is_kept_in_the_vault_and_not_fetched_twice() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    let before = mock.commands().iter().filter(|c| c.contains("BODY.PEEK[]")).count();
    mock.source.body(id).await.unwrap();
    mock.source.body(id).await.unwrap();
    let path = beavermail_vault::message_path(&mock.account.key(), "INBOX", 1, 5);
    let context = format!("{}/INBOX/1-5", mock.account.key());
    assert!(mock.files.read(&path, &context).unwrap().is_some(), "the message is stored under its UID");
    let fetched = mock.commands().iter().filter(|c| c.contains("BODY.PEEK[]")).count() - before;
    assert_eq!(fetched, 1, "the second read comes from the file");
    // Deleting the message on the server removes the file with it.
    mock.source.archive(id).await.unwrap();
    let path = beavermail_vault::message_path(&mock.account.key(), "INBOX", 1, 5);
    let context = format!("{}/INBOX/1-5", mock.account.key());
    assert_eq!(mock.files.read(&path, &context).unwrap(), None, "the file went with the message");
}
