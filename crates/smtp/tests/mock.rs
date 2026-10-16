// SPDX-License-Identifier: AGPL-3.0-only

//! The SMTP backend against a scripted local server that records the conversation.

use std::sync::{Arc, Mutex};

use beavermail_core::{Account, MailError, MailSender, Outgoing, Server, Tls};
use beavermail_smtp::SmtpSender;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct Transcript {
    commands: Vec<String>,
    data: String,
}

async fn start(good_password: &'static str, password: &str) -> (SmtpSender, Arc<Mutex<Transcript>>) {
    let (port, transcript) = listen(good_password).await;
    (SmtpSender::new(&account(port), password.into()), transcript)
}

/// A server on a port of its own, and what it hears.
async fn listen(good_password: &'static str) -> (u16, Arc<Mutex<Transcript>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let transcript = Arc::new(Mutex::new(Transcript::default()));
    let served = transcript.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, served.clone(), good_password));
        }
    });
    (port, transcript)
}

fn account(port: u16) -> Account {
    let server = Server { host: "127.0.0.1".into(), port, tls: Tls::None };
    Account { name: "mock".into(), user: "me@example.com".into(), email: String::new(), imap: server.clone(), smtp: server, save_sent: true, keep: beavermail_core::Keep::default(), signature: String::new(), oauth: None, smtp_user: None }
}

async fn serve(socket: TcpStream, transcript: Arc<Mutex<Transcript>>, good_password: &'static str) {
    let (read, mut write) = socket.into_split();
    let mut lines = BufReader::new(read).lines();
    let _ = write.write_all(b"220 mock ESMTP\r\n").await;
    let mut in_data = false;
    while let Ok(Some(line)) = lines.next_line().await {
        if in_data {
            if line == "." {
                in_data = false;
                let _ = write.write_all(b"250 queued\r\n").await;
            } else {
                let mut t = transcript.lock().unwrap();
                t.data.push_str(&line);
                t.data.push('\n');
            }
            continue;
        }
        transcript.lock().unwrap().commands.push(line.clone());
        let verb = line.to_ascii_uppercase();
        let reply: &[u8] = if verb.starts_with("EHLO") {
            b"250-mock\r\n250 AUTH PLAIN LOGIN XOAUTH2\r\n"
        } else if verb.starts_with("AUTH PLAIN") {
            // "\0user\0password" in base64 is hard to compare by eye, so the secret is checked decoded.
            let encoded = line.rsplit(' ').next().unwrap_or("");
            if decode(encoded).ends_with(good_password) { b"235 ok\r\n" } else { b"535 5.7.8 bad credentials\r\n" }
        } else if verb.starts_with("AUTH XOAUTH2") {
            let encoded = line.rsplit(' ').next().unwrap_or("");
            if decode(encoded) == format!("user=me@example.com\x01auth=Bearer {good_password}\x01\x01") { b"235 ok\r\n" } else { b"535 5.7.8 bad token\r\n" }
        } else if verb.starts_with("DATA") {
            in_data = true;
            b"354 go on\r\n"
        } else if verb.starts_with("QUIT") {
            b"221 bye\r\n"
        } else {
            b"250 ok\r\n"
        };
        if write.write_all(reply).await.is_err() {
            return;
        }
    }
}

/// Minimal base64 decoder for the AUTH PLAIN token (the tests need nothing else).
fn decode(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut bits, mut count, mut out) = (0u32, 0, Vec::new());
    for byte in text.bytes().filter(|b| *b != b'=') {
        bits = (bits << 6) | ALPHABET.iter().position(|a| *a == byte).unwrap_or(0) as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn message() -> Outgoing {
    Outgoing {
        from: "me@example.com".into(),
        to: vec!["Ada <ada@example.com>".into(), "grace@example.com".into()],
        cc: vec!["linus@example.com".into()],
        subject: "Lunch".into(),
        body: "See you at noon.".into(),
        ..Outgoing::default()
    }
}

#[tokio::test]
async fn delivers_to_every_recipient() {
    let (sender, transcript) = start("secret", "secret").await;
    let raw = sender.send(message()).await.unwrap();
    assert!(String::from_utf8(raw).unwrap().contains("Subject: Lunch"), "the returned bytes are the message as sent");
    let t = transcript.lock().unwrap();
    let rcpt: Vec<_> = t.commands.iter().filter(|c| c.starts_with("RCPT TO")).collect();
    assert_eq!(rcpt.len(), 3, "{:?}", t.commands);
    assert!(t.commands.iter().any(|c| c.starts_with("MAIL FROM:<me@example.com>")));
    assert!(t.data.contains("Subject: Lunch"), "{}", t.data);
    assert!(t.data.contains("See you at noon."));
    assert!(t.data.contains("Cc: linus@example.com"));
}

#[tokio::test]
async fn mail_goes_out_under_the_senders_name() {
    let (sender, transcript) = start("secret", "secret").await;
    let raw = String::from_utf8(sender.send(Outgoing { from: "\"Ada Lovelace\" <me@example.com>".into(), ..message() }).await.unwrap()).unwrap();
    let from = raw.lines().find(|l| l.starts_with("From: ")).unwrap();
    assert!(from.contains("Ada Lovelace") && from.ends_with("<me@example.com>"), "{from}");
    assert!(transcript.lock().unwrap().commands.iter().any(|c| c.starts_with("MAIL FROM:<me@example.com>")));
}

#[tokio::test]
async fn a_blind_copy_is_delivered_but_shown_only_in_the_senders_copy() {
    let (sender, transcript) = start("secret", "secret").await;
    let mut with_bcc = message();
    with_bcc.bcc = vec!["hidden@example.com".into()];
    let raw = String::from_utf8(sender.send(with_bcc).await.unwrap()).unwrap();
    let t = transcript.lock().unwrap();
    assert!(t.commands.iter().any(|c| c.starts_with("RCPT TO:<hidden@example.com>")), "{:?}", t.commands);
    assert!(!t.data.to_ascii_lowercase().contains("bcc:") && !t.data.contains("hidden@example.com"), "the recipients never see it: {}", t.data);
    assert!(raw.contains("Bcc: hidden@example.com"), "the copy for Sent keeps it: {raw}");
    let id = |text: &str| text.lines().find(|l| l.to_ascii_lowercase().starts_with("message-id:")).map(str::to_owned);
    assert_eq!(id(&raw), id(&t.data), "the copy in Sent is the same message");
}

#[tokio::test]
async fn a_provider_account_signs_in_with_an_access_token() {
    let (sender, transcript) = start("access-1", "not the password").await;
    let sender = sender.with_token(Arc::new(|| Ok("access-1".to_owned())));
    sender.send(message()).await.unwrap();
    assert!(transcript.lock().unwrap().commands.iter().any(|c| c.starts_with("AUTH XOAUTH2")));
    let failing = SmtpSender::new(&Account { oauth: Some(beavermail_core::OAuthProvider::Google), ..account(1) }, String::new()).with_token(Arc::new(|| Err(MailError::Auth("sign in again".into()))));
    assert!(matches!(failing.send(message()).await.unwrap_err(), MailError::Auth(_)), "no token: the server is never asked");
}

#[tokio::test]
async fn the_outgoing_server_signs_in_with_its_own_login() {
    let (port, transcript) = listen("relay-secret").await;
    let mut own = account(port);
    own.smtp_user = Some("relay-user".into());
    SmtpSender::new(&own, "relay-secret".into()).check().await.unwrap();
    let auth = transcript.lock().unwrap().commands.iter().find(|c| c.starts_with("AUTH PLAIN")).cloned().unwrap();
    assert!(decode(auth.rsplit(' ').next().unwrap()).contains("relay-user"), "{auth}");
}

#[tokio::test]
async fn a_check_signs_in_and_sends_nothing() {
    let (sender, transcript) = start("secret", "secret").await;
    sender.check().await.unwrap();
    assert!(transcript.lock().unwrap().commands.iter().all(|c| !c.starts_with("MAIL FROM")), "nothing sent");
    let (wrong, _) = start("secret", "wrong").await;
    assert!(matches!(wrong.check().await.unwrap_err(), MailError::Auth(_)));
    assert!(matches!(SmtpSender::new(&account(1), "secret".into()).check().await.unwrap_err(), MailError::Offline(_)));
}

#[tokio::test]
async fn a_wrong_password_is_an_auth_error() {
    let (sender, _) = start("secret", "wrong").await;
    let error = sender.send(message()).await.unwrap_err();
    assert!(matches!(error, MailError::Auth(_)), "{error:?}");
}

#[tokio::test]
async fn an_unreachable_server_is_offline_so_the_message_can_wait() {
    // Nothing listens on port 1.
    let error = SmtpSender::new(&account(1), "secret".into()).send(message()).await.unwrap_err();
    assert!(matches!(error, MailError::Offline(_)), "{error:?}");
}

#[tokio::test]
async fn working_offline_sends_nothing() {
    let (sender, transcript) = start("secret", "secret").await;
    sender.set_offline(true);
    assert!(matches!(sender.send(message()).await.unwrap_err(), MailError::Offline(_)));
    assert!(transcript.lock().unwrap().commands.is_empty(), "the server heard nothing");
}

#[tokio::test]
async fn invalid_addresses_never_reach_the_server() {
    let (sender, transcript) = start("secret", "secret").await;
    let mut bad = message();
    bad.to = vec!["nobody".into()];
    assert!(matches!(sender.send(bad).await.unwrap_err(), MailError::Backend(_)));
    assert!(transcript.lock().unwrap().commands.is_empty());
}

#[tokio::test]
async fn attachments_reach_the_server_and_come_back_in_the_sent_copy() {
    let (sender, transcript) = start("secret", "secret").await;
    let mut with = message();
    with.attachments = vec![beavermail_core::OutgoingFile { name: "plan.pdf".into(), mime: "application/pdf".into(), data: vec![0, 159, 146, 150, 37, 80, 68, 70] }];
    let raw = sender.send(with).await.unwrap();
    let sent = String::from_utf8_lossy(&raw).into_owned();
    let t = transcript.lock().unwrap();
    for text in [&t.data, &sent] {
        assert!(text.contains("multipart/mixed"), "{text}");
        assert!(text.contains("filename=\"plan.pdf\""), "{text}");
        assert!(text.contains("See you at noon."), "{text}");
    }
    // The eight bytes above, base64-encoded.
    assert!(t.data.contains("AJ+SliVQREY="), "{}", t.data);
}
