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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let transcript = Arc::new(Mutex::new(Transcript::default()));
    let served = transcript.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, served.clone(), good_password));
        }
    });
    let server = Server { host: "127.0.0.1".into(), port, tls: Tls::None };
    let account = Account { name: "mock".into(), user: "me@example.com".into(), email: String::new(), imap: server.clone(), smtp: server, save_sent: true, keep: beavermail_core::Keep::default() };
    (SmtpSender::new(&account, password.into()), transcript)
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
            b"250-mock\r\n250 AUTH PLAIN LOGIN\r\n"
        } else if verb.starts_with("AUTH PLAIN") {
            // "\0user\0password" in base64 is hard to compare by eye, so the secret is checked decoded.
            let encoded = line.rsplit(' ').next().unwrap_or("");
            if decode(encoded).ends_with(good_password) { b"235 ok\r\n" } else { b"535 5.7.8 bad credentials\r\n" }
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
async fn a_wrong_password_is_an_auth_error() {
    let (sender, _) = start("secret", "wrong").await;
    let error = sender.send(message()).await.unwrap_err();
    assert!(matches!(error, MailError::Auth(_)), "{error:?}");
}

#[tokio::test]
async fn invalid_addresses_never_reach_the_server() {
    let (sender, transcript) = start("secret", "secret").await;
    let mut bad = message();
    bad.to = vec!["nobody".into()];
    assert!(matches!(sender.send(bad).await.unwrap_err(), MailError::Backend(_)));
    assert!(transcript.lock().unwrap().commands.is_empty());
}
