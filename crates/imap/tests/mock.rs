// SPDX-License-Identifier: AGPL-3.0-only

//! The IMAP backend against a scripted local server, so writes (flags, moves, erase) can be
//! checked without touching a real mailbox. The server records every command it receives.

use std::sync::{Arc, Mutex};

use beavermail_core::{Account, Keep, MailError, MailSource, MessageId, Progress, Role, Server, Tls};
use beavermail_imap::ImapSource;
use beavermail_store::Store;
use beavermail_vault::Vault;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const HEADER: &str = "From: Ada <ada@example.com>\r\nSubject: Hello\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\n\r\n";

/// The one message the server holds, complete.
const DRAFT: &[u8] = b"From: me@example.com\r\nX-BeaverMail-Draft: d1\r\nSubject: wip\r\n\r\ntext\r\n";

const MESSAGE: &str = "From: Ada <ada@example.com>\r\nTo: Me <me@example.com>, grace@example.com\r\nCc: Linus <linus@example.com>\r\nReply-To: lists@example.com\r\nSubject: Hello\r\nMessage-ID: <m2@example.com>\r\nReferences: <m0@example.com> <m1@example.com>\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\n\r\nHi there.\r\n";

const LIST: &str = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\Archive) \"/\" \"Archive\"\r\n* LIST (\\Sent) \"/\" \"Sent\"\r\n* LIST (\\Drafts) \"/\" \"Drafts\"\r\n* LIST (\\Trash) \"/\" \"Trash\"\r\n";

const PLAIN_STRUCTURE: &str = "BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 9 1 NIL NIL NIL) ";
const MIXED_STRUCTURE: &str = "BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 9 1 NIL NIL NIL)(\"APPLICATION\" \"PDF\" (\"NAME\" \"plan.pdf\") NIL NIL \"BASE64\" 100 NIL (\"ATTACHMENT\" (\"FILENAME\" \"plan.pdf\")) NIL) \"MIXED\" (\"BOUNDARY\" \"b\") NIL NIL) ";

struct Mock {
    source: ImapSource,
    account: Account,
    store: Arc<Store>,
    files: Arc<Vault>,
    log: Arc<Mutex<Vec<String>>>,
    /// The message the server sends when asked for one.
    message: Arc<Mutex<String>>,
    /// The folders the server answers LIST with.
    list: Arc<Mutex<String>>,
    /// A command the server answers with BYE before closing the connection, as when shutting down.
    bye: Arc<Mutex<Option<&'static str>>>,
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
    start_with(move_works, false).await
}

/// A server that does not offer IDLE, so changes can only be found by asking.
async fn start_without_idle() -> Mock {
    start_opts(true, false, false).await
}

/// `attachment`: the server describes its message as having a PDF attached.
async fn start_with(move_works: bool, attachment: bool) -> Mock {
    start_opts(move_works, attachment, true).await
}

async fn start_opts(move_works: bool, attachment: bool, idle: bool) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let message = Arc::new(Mutex::new(MESSAGE.to_owned()));
    let list = Arc::new(Mutex::new(LIST.to_owned()));
    let bye = Arc::new(Mutex::new(None));
    let (served, replies) = (log.clone(), Replies { message: message.clone(), list: list.clone(), bye: bye.clone() });
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, served.clone(), move_works, attachment, idle, replies.clone()));
        }
    });
    let server = Server { host: "127.0.0.1".into(), port, tls: Tls::None };
    let account = Account { name: "mock".into(), user: "u".into(), email: String::new(), imap: server.clone(), smtp: server, save_sent: true, keep: beavermail_core::Keep::default(), signature: String::new(), oauth: None, smtp_user: None };
    let store = Arc::new(Store::in_memory().unwrap());
    let files = Arc::new(Vault::in_memory());
    Mock { source: ImapSource::new(&account, "p".into(), store.clone(), files.clone()), account, store, files, log, message, list, bye }
}

/// What a test can change in the server's answers while it runs; see [`Mock`].
#[derive(Clone)]
struct Replies {
    message: Arc<Mutex<String>>,
    list: Arc<Mutex<String>>,
    bye: Arc<Mutex<Option<&'static str>>>,
}

async fn serve(socket: TcpStream, log: Arc<Mutex<Vec<String>>>, move_works: bool, attachment: bool, idle: bool, replies: Replies) {
    let Replies { message, list, bye } = replies;
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
        if bye.lock().unwrap().is_some_and(|b| command.contains(b)) {
            let _ = write.write_all(b"* BYE [UNAVAILABLE] Server shutting down\r\n").await;
            return; // closed cleanly, the command never answered
        }
        let verb = command.to_ascii_uppercase();
        let reply = if verb.starts_with("CAPABILITY") {
            let extra = if idle { " IDLE" } else { "" };
            format!("* CAPABILITY IMAP4rev1{extra}\r\n{tag} OK done\r\n")
        } else if verb.starts_with("AUTHENTICATE XOAUTH2") {
            // The client's answer to the challenge is a line of its own, kept in the log to check.
            let mut answer = String::new();
            if write.write_all(b"+ \r\n").await.is_err() || read.read_line(&mut answer).await.is_err() {
                return;
            }
            log.lock().unwrap().push(answer.trim_end().to_owned());
            format!("{tag} OK signed in\r\n")
        } else if verb.starts_with("IDLE") {
            // Say something changed after a moment, then wait for the client to end IDLE.
            if write.write_all(b"+ idling\r\n").await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            if write.write_all(b"* 2 EXISTS\r\n").await.is_err() {
                return;
            }
            let mut ended = String::new();
            loop {
                ended.clear();
                if !matches!(read.read_line(&mut ended).await, Ok(n) if n > 0) {
                    return;
                }
                if ended.trim().eq_ignore_ascii_case("DONE") {
                    log.lock().unwrap().push("DONE".to_owned());
                    break;
                }
            }
            format!("{tag} OK IDLE terminated\r\n")
        } else if verb.starts_with("NOOP") {
            format!("{tag} OK done\r\n")
        } else if verb.starts_with("LIST") {
            format!("{}{tag} OK done\r\n", list.lock().unwrap())
        } else if verb.starts_with("STATUS") {
            let name = command.split(' ').nth(1).unwrap_or("\"INBOX\"");
            format!("* STATUS {name} (MESSAGES 1240 UNSEEN 1)\r\n{tag} OK done\r\n")
        } else if verb.starts_with("SELECT") || verb.starts_with("EXAMINE") {
            format!("* 1 EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 1] ok\r\n* OK [PERMANENTFLAGS (\\Seen \\Flagged \\Deleted \\*)] ok\r\n{tag} OK [READ-WRITE] done\r\n")
        } else if verb.contains("BODY.PEEK[]") {
            let message = message.lock().unwrap().clone();
            let n = message.len();
            format!("* 1 FETCH (UID 5 BODY[] {{{n}}}\r\n{message})\r\n{tag} OK done\r\n")
        } else if verb.contains("FETCH") && verb.contains("HEADER") {
            let n = HEADER.len();
            let structure = match (verb.contains("BODYSTRUCTURE"), attachment) {
                (false, _) => "",
                (true, false) => PLAIN_STRUCTURE,
                (true, true) => MIXED_STRUCTURE,
            };
            format!("* 1 FETCH (UID 5 FLAGS () {structure}BODY[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES)] {{{n}}}\r\n{HEADER})\r\n{tag} OK done\r\n")
        } else if verb.contains("FETCH") && verb.contains("BODYSTRUCTURE") {
            let structure = if attachment { MIXED_STRUCTURE } else { PLAIN_STRUCTURE };
            format!("* 1 FETCH (UID 5 {structure})\r\n{tag} OK done\r\n")
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
        } else if verb.starts_with("UID SEARCH DELETED") {
            // Deleted by another client and not expunged.
            format!("* SEARCH 7\r\n{tag} OK done\r\n")
        } else if verb.starts_with("UID SEARCH") {
            // Two copies of the same draft: the older (5) and the one just appended (9).
            format!("* SEARCH 5 9\r\n{tag} OK done\r\n")
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
async fn a_list_without_the_inbox_forgets_nothing() {
    let mock = start(true).await;
    message_in(&mock, "INBOX").await;
    // As a server still starting answers: no folders at all.
    mock.list.lock().unwrap().clear();
    assert!(mock.source.mailboxes().await.is_err());
    assert_eq!(mock.source.cached_mailboxes().await.unwrap().len(), 5);
    assert_eq!(mock.source.messages("INBOX").await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_list_cut_short_by_the_server_leaving_forgets_nothing() {
    let mock = start(true).await;
    message_in(&mock, "INBOX").await;
    *mock.bye.lock().unwrap() = Some("LIST");
    let error = mock.source.mailboxes().await.unwrap_err();
    assert!(matches!(error, MailError::Offline(_)), "a server leaving is offline for a while, tried again: {error:?}");
    assert_eq!(mock.source.cached_mailboxes().await.unwrap().len(), 5);
    assert_eq!(mock.source.messages("INBOX").await.unwrap().len(), 1);
    // The server is back: a new connection lists the folders again.
    *mock.bye.lock().unwrap() = None;
    assert_eq!(mock.source.mailboxes().await.unwrap().len(), 5);
}

#[tokio::test]
async fn flags_cut_short_by_the_server_leaving_forget_no_message() {
    let mock = start(true).await;
    message_in(&mock, "INBOX").await;
    *mock.bye.lock().unwrap() = Some("(UID FLAGS)");
    assert!(matches!(mock.source.sync("INBOX").await, Err(MailError::Offline(_))));
    assert_eq!(mock.source.messages("INBOX").await.unwrap().len(), 1);
}

#[tokio::test]
async fn reading_flags_are_stored() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    mock.source.set_unread(id, false).await.unwrap();
    mock.source.set_unread(id, true).await.unwrap();
    assert_eq!(mock.writes(), ["UID STORE 5 +FLAGS.SILENT (\\Seen)", "UID STORE 5 -FLAGS.SILENT (\\Seen)"]);
    mock.source.set_pinned(id, true).await.unwrap();
    assert!(mock.source.messages("INBOX").await.unwrap()[0].pinned);
    mock.source.set_pinned(id, false).await.unwrap();
    assert_eq!(mock.writes()[2..], ["UID STORE 5 +FLAGS.SILENT ($Pinned)", "UID STORE 5 -FLAGS.SILENT ($Pinned)"]);
    mock.source.set_starred(id, true).await.unwrap();
    assert!(mock.source.messages("INBOX").await.unwrap()[0].starred);
    assert_eq!(mock.writes()[4], "UID STORE 5 +FLAGS.SILENT (\\Flagged)");
    mock.source.set_label(id, "$label1", true).await.unwrap();
    assert_eq!(mock.source.messages("INBOX").await.unwrap()[0].labels, ["$label1"]);
    assert_eq!(mock.writes()[5], "UID STORE 5 +FLAGS.SILENT ($label1)");
    assert!(mock.source.set_label(id, "two words", true).await.is_err(), "never a broken command");
    // Set here at once, and kept while the server cannot be reached: the change waits to be sent.
    mock.source.set_offline(true);
    assert!(matches!(mock.source.set_starred(id, false).await, Err(MailError::Offline(_))));
    assert!(!mock.source.messages("INBOX").await.unwrap()[0].starred);
    mock.source.set_offline(false);
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
async fn a_message_moves_to_any_folder_by_name_but_not_to_one_gone() {
    let mock = start(true).await;
    mock.list.lock().unwrap().push_str("* LIST (\\HasNoChildren) \"/\" \"Work/Clients\"\r\n");
    mock.source.mailboxes().await.unwrap();
    let id = message_in(&mock, "INBOX").await;
    mock.source.move_to_folder(id, "Work/Clients").await.unwrap();
    assert!(mock.source.messages("INBOX").await.unwrap().is_empty(), "gone from the Inbox at once");
    let id = message_in(&mock, "INBOX").await;
    let error = mock.source.move_to_folder(id, "Old").await.unwrap_err();
    assert!(matches!(&error, MailError::Backend(why) if why.contains("Old")), "{error:?}");
    assert_eq!(mock.writes(), ["UID MOVE 5 \"Work/Clients\""], "nothing sent for a folder that is not there");
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
async fn emptying_erases_the_known_messages_of_that_folder_only() {
    let mock = start(true).await;
    message_in(&mock, "Trash").await;
    assert!(mock.source.empty(Role::Inbox).await.is_err(), "only Trash and Spam");
    assert_eq!(mock.source.empty(Role::Trash).await.unwrap(), 1);
    // With what another client already deleted (7), never left behind.
    assert_eq!(mock.writes(), ["UID STORE 5,7 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 5,7"]);
    assert!(mock.source.messages("Trash").await.unwrap().is_empty());
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
async fn working_offline_keeps_off_the_server_until_online_again() {
    let mock = start(true).await;
    mock.source.sync("INBOX").await.unwrap(); // a session is open now
    mock.source.set_offline(true);
    assert!(matches!(mock.source.sync("INBOX").await, Err(MailError::Offline(_))), "the open session is not used");
    assert!(mock.source.messages("INBOX").await.is_ok(), "the archive still answers");
    mock.source.set_offline(false);
    mock.source.sync("INBOX").await.unwrap();
}

#[tokio::test]
async fn a_provider_account_signs_in_with_an_access_token() {
    let mock = start(true).await;
    let source = ImapSource::new(&mock.account, String::new(), mock.store.clone(), mock.files.clone()).with_token(Arc::new(|| Ok("access-1".to_owned())));
    source.sync("INBOX").await.unwrap();
    let commands = mock.commands();
    assert!(!commands.iter().any(|c| c.starts_with("LOGIN")), "{commands:?}");
    // "user=u^Aauth=Bearer access-1^A^A" in base64.
    assert!(commands.iter().any(|c| c == "dXNlcj11AWF1dGg9QmVhcmVyIGFjY2Vzcy0xAQE="), "{commands:?}");
}

#[tokio::test]
async fn a_check_signs_in_and_out_and_says_why_a_server_cannot_be_reached() {
    let mock = start(true).await;
    mock.source.check().await.unwrap();
    let commands = mock.commands();
    assert!(commands.iter().any(|c| c.starts_with("LOGIN")) && commands.iter().any(|c| c.starts_with("LOGOUT")), "{commands:?}");
    assert!(!commands.iter().any(|c| c.starts_with("SELECT") || c.starts_with("LIST")), "nothing but the login: {commands:?}");
    assert!(mock.store.folders(&mock.account.key()).unwrap().is_empty(), "nothing stored");
    let mut account = mock.account.clone();
    account.imap.port = 1; // nothing listens there
    let error = ImapSource::new(&account, "p".into(), mock.store.clone(), mock.files.clone()).check().await.unwrap_err();
    assert!(matches!(&error, MailError::Offline(why) if why.contains("refused the connection on port 1")), "{error:?}");
    account.imap.host = "no-such-host.invalid".into();
    let error = ImapSource::new(&account, "p".into(), mock.store.clone(), mock.files.clone()).check().await.unwrap_err();
    assert!(matches!(&error, MailError::Offline(why) if why.contains("could not be found")), "{error:?}");
}

#[tokio::test]
async fn an_encrypted_message_opens_with_the_accounts_key_and_its_text_is_not_indexed() {
    let mock = start(true).await;
    let (mine, ada) = (beavermail_crypto::generate("", "u").unwrap(), beavermail_crypto::generate("Ada", "ada@example.com").unwrap());
    let plain = "From: Ada <ada@example.com>\r\nTo: u\r\nSubject: Hello\r\nContent-Type: text/plain\r\n\r\nThe launch code is zebra.\r\n";
    let sealed = beavermail_crypto::encrypt(plain.as_bytes(), &ada, &[beavermail_crypto::public_key(&mine).unwrap()], &[]).unwrap();
    *mock.message.lock().unwrap() = String::from_utf8(sealed).unwrap();
    let id = message_in(&mock, "INBOX").await;
    // Without the key: said so, nothing shown.
    let locked = mock.source.body(id).await.unwrap();
    assert_eq!(locked.encryption, Some(beavermail_core::Encryption::NoKey(beavermail_core::SignedWith::OpenPgp)));
    assert!(locked.text.is_empty() && locked.attachments.is_empty());
    mock.files.write(&beavermail_crypto::key_path("u"), &mine).unwrap();
    let opened = mock.source.body(id).await.unwrap();
    assert_eq!(opened.encryption, Some(beavermail_core::Encryption::Opened(beavermail_core::SignedWith::OpenPgp)));
    assert!(opened.text.contains("The launch code is zebra."), "{}", opened.text);
    assert!(mock.store.search(&mock.account.key(), "zebra", 10).unwrap().is_empty(), "what was decrypted is never indexed");
}

#[tokio::test]
async fn an_smime_encrypted_message_opens_with_the_accounts_certificate() {
    let mock = start(true).await;
    let fixtures = format!("{}/../crypto/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let certificate = beavermail_crypto::import_certificate(&std::fs::read(format!("{fixtures}/ada.p12")).unwrap(), Some("p12pass")).unwrap();
    *mock.message.lock().unwrap() = String::from_utf8(std::fs::read(format!("{fixtures}/smime-encrypted-aes256.eml")).unwrap()).unwrap();
    let id = message_in(&mock, "INBOX").await;
    mock.files.write(&beavermail_crypto::certificate_path("u"), &certificate).unwrap();
    let opened = mock.source.body(id).await.unwrap();
    assert_eq!(opened.encryption, Some(beavermail_core::Encryption::Opened(beavermail_core::SignedWith::Smime)));
    assert!(opened.text.contains("See you at noon."), "{}", opened.text);
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
    assert_eq!(offline.cached_mailboxes().await.unwrap().len(), 5);
    assert_eq!(offline.messages("INBOX").await.unwrap().len(), 1);
    assert_eq!(offline.body(id).await.unwrap(), body, "an opened message is readable offline");
    assert!(matches!(offline.sync("INBOX").await, Err(MailError::Offline(_))), "an unreachable server is told apart from other failures");
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
    assert!(mock.files.read(&path).unwrap().is_some(), "the message is stored under its UID");
    let fetched = mock.commands().iter().filter(|c| c.contains("BODY.PEEK[]")).count() - before;
    assert_eq!(fetched, 1, "the second read comes from the file");
    // Deleting the message on the server removes the file with it.
    mock.source.archive(id).await.unwrap();
    let path = beavermail_vault::message_path(&mock.account.key(), "INBOX", 1, 5);
    assert_eq!(mock.files.read(&path).unwrap(), None, "the file went with the message");
}

#[tokio::test]
async fn a_locked_vault_still_opens_messages_from_the_server() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    // A vault protected by a passphrase that was not entered: nothing can be stored or read.
    let dir = std::env::temp_dir().join(format!("beavermail-imap-locked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = std::sync::Arc::new(beavermail_vault::MemoryStore::default());
    struct Shared(std::sync::Arc<beavermail_vault::MemoryStore>);
    impl beavermail_vault::CredentialStore for Shared {
        fn get(&self, id: &str) -> Result<Option<Vec<u8>>, MailError> { self.0.get(id) }
        fn set(&self, id: &str, secret: &[u8]) -> Result<(), MailError> { self.0.set(id, secret) }
        fn remove(&self, id: &str) -> Result<(), MailError> { self.0.remove(id) }
    }
    let vault = Vault::open(&dir, Box::new(Shared(store.clone()))).unwrap();
    vault.set_mode(beavermail_vault::Mode::Passphrase, Some("a long passphrase"), &|_, _| {}).unwrap();
    let locked = Vault::open(&dir, Box::new(Shared(store))).unwrap();
    assert!(locked.is_locked());
    let source = ImapSource::new(&mock.account, "p".into(), mock.store.clone(), Arc::new(locked));
    let body = source.body(id).await.expect("the message still opens, straight from the server");
    assert_eq!(body.text.trim(), "Hi there.");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_background_download_stores_what_the_window_asks_for_and_reports_progress() {
    use std::sync::atomic::Ordering::Relaxed;
    let mock = start(true).await;
    mock.source.mailboxes().await.unwrap();
    let path = beavermail_vault::message_path(&mock.account.key(), "INBOX", 1, 5);

    // The only message is from January 2024: outside a 30-day window, so nothing is fetched.
    let progress = Progress::default();
    mock.source.download(Keep::Month, &progress).await.unwrap();
    assert_eq!(progress.total.load(Relaxed), 0);
    assert_eq!(mock.files.read(&path).unwrap(), None);
    assert!(!mock.commands().iter().any(|c| c.contains("BODY.PEEK[]")));

    // "Opened only" never downloads.
    mock.source.download(Keep::Opened, &progress).await.unwrap();
    assert!(!mock.commands().iter().any(|c| c.contains("BODY.PEEK[]")));

    // "Everything" does, and counts.
    let progress = Progress::default();
    mock.source.download(Keep::All, &progress).await.unwrap();
    // The scripted server answers every folder with the same message: INBOX, Archive, Sent and Drafts; Trash is skipped.
    assert_eq!((progress.done.load(Relaxed), progress.total.load(Relaxed)), (4, 4));
    assert!(mock.files.read(&path).unwrap().is_some());
    // Run again: nothing is left to fetch.
    let fetches = mock.commands().iter().filter(|c| c.contains("BODY.PEEK[]")).count();
    mock.source.download(Keep::All, &Progress::default()).await.unwrap();
    assert_eq!(mock.commands().iter().filter(|c| c.contains("BODY.PEEK[]")).count(), fetches);
    // The list preview came with the download.
    assert_eq!(mock.source.messages("INBOX").await.unwrap()[0].preview, "Hi there.");
}

#[tokio::test]
async fn a_cancelled_download_stops_before_fetching() {
    let mock = start(true).await;
    mock.source.mailboxes().await.unwrap();
    let progress = Progress::default();
    progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    mock.source.download(Keep::All, &progress).await.unwrap();
    assert!(!mock.commands().iter().any(|c| c.contains("BODY.PEEK[]")));
}

#[tokio::test]
async fn folders_report_how_many_messages_the_server_holds_and_how_many_are_stored() {
    let mock = start(true).await;
    let listed = mock.source.mailboxes().await.unwrap();
    assert_eq!(listed.iter().find(|m| m.name == "INBOX").unwrap().total, 1240);
    // Offline, the archive still knows the server's count; nothing is stored yet.
    let cached = mock.source.cached_mailboxes().await.unwrap();
    let inbox = cached.iter().find(|m| m.name == "INBOX").unwrap();
    assert_eq!((inbox.total, inbox.stored), (1240, 0));
    mock.source.download(Keep::All, &Progress::default()).await.unwrap();
    let inbox = mock.source.cached_mailboxes().await.unwrap().into_iter().find(|m| m.name == "INBOX").unwrap();
    assert_eq!((inbox.total, inbox.stored), (1240, 1));
}

#[tokio::test]
async fn the_list_knows_which_messages_carry_a_file_from_the_servers_structure() {
    let with = start_with(true, true).await;
    with.source.mailboxes().await.unwrap();
    with.source.sync("INBOX").await.unwrap();
    assert!(with.source.messages("INBOX").await.unwrap()[0].has_attachments);

    let without = start_with(true, false).await;
    without.source.mailboxes().await.unwrap();
    without.source.sync("INBOX").await.unwrap();
    assert!(!without.source.messages("INBOX").await.unwrap()[0].has_attachments);
}

#[tokio::test]
async fn opening_a_message_settles_its_attachment_flag_from_the_message_itself() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    assert!(!mock.source.messages("INBOX").await.unwrap()[0].has_attachments);
    mock.source.body(id).await.unwrap();
    assert!(!mock.source.messages("INBOX").await.unwrap()[0].has_attachments, "the mock's message has none");
}

#[tokio::test]
async fn saving_a_draft_stores_it_flagged_and_replaces_the_older_copy() {
    let mock = start(true).await;
    mock.source.mailboxes().await.unwrap();
    mock.source.save_draft("d1", DRAFT.to_vec()).await.unwrap();
    let commands = mock.commands();
    let position = |needle: &str| commands.iter().position(|c| c.contains(needle)).unwrap_or_else(|| panic!("no {needle:?} in {commands:?}"));
    assert!(commands[position("APPEND")].starts_with("APPEND \"Drafts\" (\\Draft \\Seen)"), "{commands:?}");
    assert!(position("APPEND") < position("UID SEARCH"), "search for the copies after storing the new one");
    assert!(commands.iter().any(|c| c == "UID SEARCH HEADER X-BeaverMail-Draft d1"), "{commands:?}");
    assert!(commands.iter().any(|c| c.starts_with("UID STORE 5 +FLAGS.SILENT (\\Deleted)")), "the older copy goes: {commands:?}");
    assert!(commands.iter().any(|c| c == "UID EXPUNGE 5"), "{commands:?}");
    assert!(!commands.iter().any(|c| c.contains("UID STORE 9") || c.contains("EXPUNGE 9")), "the new copy stays: {commands:?}");
}

#[tokio::test]
async fn deleting_a_draft_removes_every_copy() {
    let mock = start(true).await;
    mock.source.mailboxes().await.unwrap();
    mock.source.delete_draft("d1").await.unwrap();
    let commands = mock.commands();
    assert!(commands.iter().any(|c| c.starts_with("UID STORE 5,9 +FLAGS.SILENT (\\Deleted)")) || commands.iter().any(|c| c.starts_with("UID STORE 9,5")), "{commands:?}");
    assert!(commands.iter().any(|c| c == "UID EXPUNGE 5,9" || c == "UID EXPUNGE 9,5"), "{commands:?}");
}

#[tokio::test]
async fn an_account_without_a_drafts_folder_says_so_instead_of_guessing() {
    let mock = start(true).await;
    // Nothing listed yet and the scripted server's list has one, so make the archive forget it.
    mock.store.set_folders(&mock.account.key(), &[("INBOX".into(), Role::Inbox)]).unwrap();
    let error = mock.source.save_draft("d1", DRAFT.to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("Drafts"), "{error}");
}

#[tokio::test]
async fn idle_reports_what_the_server_says_and_stops_when_asked() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let mock = start(true).await;
    let (stop, changes) = (AtomicBool::new(false), AtomicUsize::new(0));
    let changed = || {
        changes.fetch_add(1, Relaxed);
        stop.store(true, Relaxed); // one report is enough for the test
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), mock.source.watch("INBOX", &changed, &stop)).await.expect("the watcher stops").unwrap();
    assert_eq!(changes.load(Relaxed), 1);
    let commands = mock.commands();
    assert!(commands.iter().any(|c| c == "IDLE"), "{commands:?}");
    assert!(commands.iter().any(|c| c.starts_with("EXAMINE")), "the folder is only watched, never changed: {commands:?}");
    assert!(commands.iter().any(|c| c == "DONE"), "IDLE is ended properly: {commands:?}");
}

#[tokio::test]
async fn a_watcher_told_to_stop_before_it_starts_returns_at_once() {
    let mock = start(true).await;
    let stop = std::sync::atomic::AtomicBool::new(true);
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.source.watch("INBOX", &|| panic!("nothing changed"), &stop)).await.unwrap().unwrap();
}

#[tokio::test]
async fn without_idle_the_server_is_polled() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let mock = start_without_idle().await;
    let source = ImapSource::new(&mock.account, "p".into(), mock.store.clone(), mock.files.clone()).with_poll_interval(std::time::Duration::from_millis(300));
    let (stop, changes) = (AtomicBool::new(false), AtomicUsize::new(0));
    let changed = || {
        changes.fetch_add(1, Relaxed);
        stop.store(true, Relaxed);
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), source.watch("INBOX", &changed, &stop)).await.expect("the watcher stops").unwrap();
    assert_eq!(changes.load(Relaxed), 1);
    let commands = mock.commands();
    assert!(!commands.iter().any(|c| c == "IDLE"), "{commands:?}");
    assert!(commands.iter().any(|c| c == "NOOP"), "{commands:?}");
}

#[tokio::test]
async fn the_text_of_downloaded_messages_is_searchable_and_so_are_the_headers() {
    let mock = start(true).await;
    mock.source.mailboxes().await.unwrap();
    mock.source.sync("INBOX").await.unwrap();
    assert!(mock.source.search("hi there", 10).await.unwrap().is_empty(), "the text is not downloaded yet");
    assert_eq!(mock.source.search("hello from:ada", 10).await.unwrap().len(), 1, "sender and subject are");
    mock.source.download(Keep::All, &Progress::default()).await.unwrap();
    let hits = mock.source.search("there", 10).await.unwrap();
    assert!(hits.iter().any(|h| h.folder == "INBOX"), "{hits:?}");
}

#[tokio::test]
async fn messages_stored_before_indexing_existed_get_indexed_by_the_next_run() {
    let mock = start(true).await;
    let id = message_in(&mock, "INBOX").await;
    mock.source.body(id).await.unwrap(); // stores the file and indexes the text
    assert_eq!(mock.source.search("there", 10).await.unwrap().len(), 1);
    // Forget the index, as a database from an earlier version would not have it.
    mock.store.forget_index_for_tests();
    assert!(mock.source.search("there", 10).await.unwrap().is_empty());
    mock.source.download(Keep::Opened, &Progress::default()).await.unwrap();
    assert_eq!(mock.source.search("there", 10).await.unwrap().len(), 1, "the stored file was read again");
}
