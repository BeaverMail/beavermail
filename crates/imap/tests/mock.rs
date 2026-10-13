// SPDX-License-Identifier: AGPL-3.0-only

//! The IMAP backend against a scripted local server, so writes (flags, moves, erase) can be
//! checked without touching a real mailbox. The server records every command it receives.

use std::sync::{Arc, Mutex};

use beavermail_core::{Account, MailError, MailSource, MessageId, Role, Server, Tls};
use beavermail_imap::ImapSource;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const HEADER: &str = "From: Ada <ada@example.com>\r\nSubject: Hello\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\n\r\n";

struct Mock {
    source: ImapSource,
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
    let account = Account { name: "mock".into(), user: "u".into(), imap: server.clone(), smtp: server };
    Mock { source: ImapSource::new(&account, "p".into()), log }
}

async fn serve(socket: TcpStream, log: Arc<Mutex<Vec<String>>>, move_works: bool) {
    let (read, mut write) = socket.into_split();
    let mut lines = BufReader::new(read).lines();
    let _ = write.write_all(b"* OK ready\r\n").await;
    while let Ok(Some(line)) = lines.next_line().await {
        let Some((tag, command)) = line.split_once(' ') else { continue };
        log.lock().unwrap().push(command.to_owned());
        let verb = command.to_ascii_uppercase();
        let reply = if verb.starts_with("LIST") {
            format!(
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\Archive) \"/\" \"Archive\"\r\n* LIST (\\Trash) \"/\" \"Trash\"\r\n{tag} OK done\r\n"
            )
        } else if verb.starts_with("STATUS") {
            let name = command.split(' ').nth(1).unwrap_or("\"INBOX\"");
            format!("* STATUS {name} (UNSEEN 1)\r\n{tag} OK done\r\n")
        } else if verb.starts_with("SELECT") || verb.starts_with("EXAMINE") {
            format!("* 1 EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 1] ok\r\n{tag} OK [READ-WRITE] done\r\n")
        } else if verb.contains("FETCH") {
            let n = HEADER.len();
            format!("* 1 FETCH (UID 5 FLAGS () BODY[HEADER] {{{n}}}\r\n{HEADER})\r\n{tag} OK done\r\n")
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
    mock.source.messages(mailbox).await.expect("list messages").first().expect("one message").id
}

#[tokio::test]
async fn folders_get_their_roles() {
    let mock = start(true).await;
    let mailboxes = mock.source.mailboxes().await.unwrap();
    let role = |name: &str| mailboxes.iter().find(|m| m.name == name).unwrap().role;
    assert_eq!(mailboxes[0].name, "INBOX");
    assert_eq!(role("INBOX"), Role::Other);
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
