// SPDX-License-Identifier: AGPL-3.0-only

//! SMTP backend: implements [`MailSender`] over one account's outgoing server.

use beavermail_core::{Account, MailError, MailSender, Outgoing, Server, Tls};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

pub struct SmtpSender {
    server: Server,
    user: String,
    password: String,
}

impl SmtpSender {
    /// Creates the sender; the connection is opened for each message.
    pub fn new(account: &Account, password: String) -> Self {
        Self { server: account.smtp.clone(), user: account.user.clone(), password }
    }

    fn transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>, MailError> {
        let Server { host, port, tls } = &self.server;
        let builder = match tls {
            Tls::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)),
            Tls::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
            Tls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        }
        .map_err(backend)?;
        Ok(builder.port(*port).credentials(Credentials::new(self.user.clone(), self.password.clone())).build())
    }
}

fn backend(e: impl std::fmt::Display) -> MailError {
    MailError::Backend(e.to_string())
}

fn address(text: &str) -> Result<Mailbox, MailError> {
    text.trim().parse().map_err(|_| backend(format!("{:?} is not a valid email address", text.trim())))
}

/// Builds the RFC 5322 message, checking every address.
fn build(message: &Outgoing) -> Result<Message, MailError> {
    let mut builder = Message::builder().from(address(&message.from)?).subject(message.subject.clone());
    if message.to.is_empty() {
        return Err(backend("add at least one recipient"));
    }
    for to in &message.to {
        builder = builder.to(address(to)?);
    }
    for cc in &message.cc {
        builder = builder.cc(address(cc)?);
    }
    if let Some(id) = &message.in_reply_to {
        builder = builder.in_reply_to(bracketed(id));
    }
    if !message.references.is_empty() {
        let refs: Vec<String> = message.references.iter().map(|r| bracketed(r)).collect();
        builder = builder.references(refs.join(" "));
    }
    builder.date_now().message_id(None).body(message.body.clone()).map_err(backend)
}

/// A Message-ID as it appears in a header: inside angle brackets.
fn bracketed(id: &str) -> String {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>');
    format!("<{id}>")
}

impl MailSender for SmtpSender {
    async fn send(&self, message: Outgoing) -> Result<Vec<u8>, MailError> {
        let email = build(&message)?;
        let raw = email.formatted();
        match self.transport()?.send(email).await {
            Ok(_) => Ok(raw),
            Err(e) if e.status().is_some_and(|code| code.to_string() == "535") => Err(MailError::Auth(e.to_string())),
            Err(e) => Err(backend(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outgoing() -> Outgoing {
        Outgoing {
            from: "me@example.com".into(),
            to: vec!["Ada <ada@example.com>".into()],
            subject: "Hi".into(),
            body: "Hello".into(),
            ..Outgoing::default()
        }
    }

    #[test]
    fn a_valid_message_builds() {
        assert!(build(&outgoing()).is_ok());
    }

    #[test]
    fn replies_carry_threading_headers() {
        let mut reply = outgoing();
        reply.in_reply_to = Some("b@host".into());
        reply.references = vec!["a@host".into(), "<b@host>".into()];
        let text = String::from_utf8(build(&reply).unwrap().formatted()).unwrap();
        assert!(text.contains("In-Reply-To: <b@host>"), "{text}");
        assert!(text.contains("References: <a@host> <b@host>"), "{text}");
        assert!(text.contains("Message-ID: <"), "{text}");
        assert!(text.contains("Date: "), "{text}");
    }

    #[test]
    fn bad_addresses_and_missing_recipients_are_rejected() {
        let mut bad = outgoing();
        bad.to = vec!["not an address".into()];
        assert!(build(&bad).is_err());
        let mut none = outgoing();
        none.to.clear();
        assert!(build(&none).is_err());
    }
}
