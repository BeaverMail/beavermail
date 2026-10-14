// SPDX-License-Identifier: AGPL-3.0-only

//! SMTP backend: implements [`MailSender`] over one account's outgoing server.

use beavermail_core::{Account, MailError, MailSender, Outgoing, Server, Tls};
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
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
    let builder = builder.date_now().message_id(None);
    with_content(builder, message)
}

/// Sets the body: plain text, or Markdown plus its HTML, with the attachments after it.
fn with_content(builder: lettre::message::MessageBuilder, message: &Outgoing) -> Result<Message, MailError> {
    let text = || MultiPart::alternative_plain_html(message.body.clone(), markdown_html(&message.body));
    if message.attachments.is_empty() {
        return if message.rich { builder.multipart(text()) } else { builder.body(message.body.clone()) }.map_err(backend);
    }
    let mut parts = if message.rich { MultiPart::mixed().multipart(text()) } else { MultiPart::mixed().singlepart(SinglePart::plain(message.body.clone())) };
    for file in &message.attachments {
        let kind = ContentType::parse(&file.mime).unwrap_or_else(|_| ContentType::parse("application/octet-stream").expect("a valid type"));
        parts = parts.singlepart(Attachment::new(file.name.clone()).body(file.data.clone(), kind));
    }
    builder.multipart(parts).map_err(backend)
}

/// The HTML for a Markdown message. Raw HTML in the text is shown as text, images are not sent
/// (their description is), and only web and mail links stay links: nothing remote, nothing active.
pub fn markdown_html(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut dropped_link = false;
    let events = Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH).filter_map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Some(Event::Text(text)),
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        Event::Start(Tag::Link { ref dest_url, .. }) if !["http://", "https://", "mailto:"].iter().any(|s| dest_url.to_ascii_lowercase().starts_with(s)) => {
            dropped_link = true;
            None
        }
        Event::End(TagEnd::Link) if std::mem::take(&mut dropped_link) => None,
        other => Some(other),
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

/// Marks the copy of a draft kept on the server, so that a later save can find and replace it.
#[derive(Clone)]
struct DraftId(String);

impl Header for DraftId {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii("X-BeaverMail-Draft".to_owned()).expect("a valid header name")
    }

    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(s.trim().to_owned()))
    }

    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), self.0.clone())
    }
}

/// The Message-ID every saved copy of draft `id` carries.
pub fn draft_message_id(id: &str) -> String {
    format!("{id}@beavermail.draft")
}

/// The message as it is kept in the server's Drafts folder: the same as one to be sent, but a draft
/// may be unfinished, so addresses that do not parse are left out and no recipient is needed.
pub fn render_draft(message: &Outgoing, id: &str) -> Result<Vec<u8>, MailError> {
    let mut builder = Message::builder().from(address(&message.from)?).subject(message.subject.clone()).header(DraftId(id.to_owned()));
    let recipients: Vec<Mailbox> = message.to.iter().filter_map(|a| a.trim().parse::<Mailbox>().ok()).collect();
    // The message builder wants a recipient; a draft without one gets a stand-in that is removed again below.
    let none_yet = recipients.is_empty() && message.cc.iter().all(|a| a.trim().parse::<Mailbox>().is_err());
    for to in recipients {
        builder = builder.to(to);
    }
    if none_yet {
        builder = builder.to("draft@beavermail.invalid".parse::<Mailbox>().map_err(backend)?);
    }
    for cc in message.cc.iter().filter_map(|a| a.trim().parse::<Mailbox>().ok()) {
        builder = builder.cc(cc);
    }
    if let Some(reply) = &message.in_reply_to {
        builder = builder.in_reply_to(bracketed(reply));
    }
    if !message.references.is_empty() {
        let refs: Vec<String> = message.references.iter().map(|r| bracketed(r)).collect();
        builder = builder.references(refs.join(" "));
    }
    let builder = builder.date_now().message_id(Some(bracketed(&draft_message_id(id))));
    let raw = with_content(builder, message)?.formatted();
    Ok(if none_yet { without_header(&raw, "To") } else { raw })
}

/// The message without the header `name` (and its continuation lines); the body is untouched.
fn without_header(raw: &[u8], name: &str) -> Vec<u8> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").map_or(raw.len(), |p| p + 2);
    let (head, rest) = raw.split_at(end);
    let prefix = format!("{}:", name.to_ascii_lowercase());
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    for line in head.split_inclusive(|b| *b == b'\n') {
        let continued = line.first().is_some_and(|b| *b == b' ' || *b == b'\t');
        if !continued {
            skipping = String::from_utf8_lossy(line).to_ascii_lowercase().starts_with(&prefix);
        }
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(rest);
    out
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
    fn a_draft_renders_even_when_unfinished_and_carries_its_marker_and_id() {
        let mut draft = outgoing();
        draft.to = vec!["Ada <ada@example.com>".into(), "half typed".into()];
        draft.subject = String::new();
        let text = String::from_utf8(render_draft(&draft, "abc-1").unwrap()).unwrap();
        assert!(text.contains("X-BeaverMail-Draft: abc-1"), "{text}");
        assert!(text.contains("Message-ID: <abc-1@beavermail.draft>"), "{text}");
        assert!(text.contains("ada@example.com") && !text.contains("half typed"), "only addresses that parse are kept: {text}");
        draft.to.clear();
        let bare = String::from_utf8(render_draft(&draft, "abc-2").unwrap()).unwrap();
        assert!(!bare.contains("To:") && !bare.contains("invalid") && bare.contains("Hello"), "no recipient is fine for a draft: {bare}");
        draft.from = "not an address".into();
        assert!(render_draft(&draft, "abc-3").is_err(), "the sender must be valid");
        draft.from = "me@example.com".into();
        draft.attachments = vec![beavermail_core::OutgoingFile { name: "a.txt".into(), mime: "text/plain".into(), data: b"x".to_vec() }];
        assert!(String::from_utf8(render_draft(&draft, "abc-4").unwrap()).unwrap().contains("multipart/mixed"));
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
    fn attachments_become_parts_of_a_mixed_message_with_the_text_first() {
        let mut with = outgoing();
        with.attachments = vec![
            beavermail_core::OutgoingFile { name: "plan.pdf".into(), mime: "application/pdf".into(), data: b"%PDF-1.4\x00\xff\xfe binary".to_vec() },
            beavermail_core::OutgoingFile { name: "notes.txt".into(), mime: "not a type".into(), data: b"remember".to_vec() },
        ];
        let text = String::from_utf8(build(&with).unwrap().formatted()).unwrap();
        assert!(text.contains("Content-Type: multipart/mixed"), "{text}");
        assert!(text.find("Hello").unwrap() < text.find("plan.pdf").unwrap(), "the text comes first");
        assert!(text.contains("Content-Type: application/pdf"), "{text}");
        assert!(text.contains("filename=\"plan.pdf\""), "{text}");
        assert!(text.contains("Content-Transfer-Encoding: base64"), "binary data is base64: {text}");
        assert!(text.contains("application/octet-stream"), "an unknown type falls back to plain bytes: {text}");
    }

    #[test]
    fn markdown_is_sent_as_text_and_safe_html() {
        let mut rich = outgoing();
        rich.rich = true;
        rich.body = "# Hi\n\n**bold** <script>x</script> ![pic](http://t.example/p.png) [ok](https://a.example) [bad](javascript:alert(1))".into();
        let text = String::from_utf8(build(&rich).unwrap().formatted()).unwrap();
        assert!(text.contains("multipart/alternative") && text.contains("text/plain") && text.contains("text/html"), "{text}");
        let html = markdown_html(&rich.body);
        assert!(html.contains("<h1>Hi</h1>") && html.contains("<strong>bold</strong>") && html.contains("href=\"https://a.example\""), "{html}");
        assert!(!html.contains("<script") && !html.contains("<img") && !html.contains("javascript:") && !html.contains("t.example"), "{html}");
        rich.attachments = vec![beavermail_core::OutgoingFile { name: "a.txt".into(), mime: "text/plain".into(), data: b"x".to_vec() }];
        let mixed = String::from_utf8(render_draft(&rich, "d").unwrap()).unwrap();
        assert!(mixed.find("multipart/mixed").unwrap() < mixed.find("multipart/alternative").unwrap(), "{mixed}");
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
