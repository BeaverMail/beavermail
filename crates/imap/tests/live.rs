// SPDX-License-Identifier: AGPL-3.0-only

//! Read-only checks against the account in `.env`. Skipped when `.env` is absent.

use beavermail_core::{Account, MailSource, Server, Tls};
use beavermail_imap::ImapSource;

/// Reads `IMAP_HOST`, `IMAP_PORT`, `IMAP_TLS`, `MAIL_USER` and `MAIL_PASSWORD`.
fn account_from_env() -> Result<(Account, String), String> {
    let var = |name: &str| std::env::var(name).map_err(|_| format!("{name} is not set"));
    let tls = match var("IMAP_TLS")?.trim() {
        "none" => Tls::None,
        "starttls" => Tls::StartTls,
        "tls" => Tls::Tls,
        other => return Err(format!("IMAP_TLS: unknown mode {other:?}")),
    };
    let imap = Server {
        host: var("IMAP_HOST")?,
        port: var("IMAP_PORT")?.trim().parse().map_err(|_| "IMAP_PORT is not a number".to_owned())?,
        tls,
    };
    let account = Account { name: "test".into(), user: var("MAIL_USER")?, smtp: imap.clone(), imap };
    Ok((account, var("MAIL_PASSWORD")?))
}

#[tokio::test]
async fn reads_inbox() {
    let _ = dotenvy::from_path("../../.env");
    let Ok((account, password)) = account_from_env() else {
        eprintln!("skipped: no IMAP settings in the environment");
        return;
    };
    let source = ImapSource::new(&account, password);

    let mailboxes = source.mailboxes().await.expect("list mailboxes");
    assert!(mailboxes.iter().any(|m| m.name == "INBOX"), "no INBOX in {mailboxes:?}");

    let messages = source.messages("INBOX").await.expect("list INBOX");
    eprintln!("INBOX: {} messages", messages.len());
    for m in &messages {
        let body = source.body(m.id).await.expect("fetch body");
        eprintln!("{:?} from={:?} unread={} received={:?} body={} bytes", m.subject, m.from, m.unread, m.received, body.len());
    }
    assert!(source.messages("no/such/box").await.is_err());
}
