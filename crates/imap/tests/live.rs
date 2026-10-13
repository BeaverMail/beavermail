// SPDX-License-Identifier: AGPL-3.0-only

//! Read-only checks against the account in `.env`. Skipped when `.env` is absent.

use beavermail_core::MailSource;
use beavermail_imap::{Config, ImapSource};

#[tokio::test]
async fn reads_inbox() {
    let _ = dotenvy::from_path("../../.env");
    let Ok(config) = Config::from_env() else {
        eprintln!("skipped: no IMAP settings in the environment");
        return;
    };
    let source = ImapSource::new(config);

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
