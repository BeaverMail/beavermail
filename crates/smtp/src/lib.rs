// SPDX-License-Identifier: AGPL-3.0-only

//! SMTP backend: implements [`MailSender`] over one account's outgoing server.

use beavermail_core::{Account, MailError, MailSender, Outgoing, Server, Tls};
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

pub struct SmtpSender {
    server: Server,
    user: String,
    password: String,
    /// The user chose to work offline: sending answers [`MailError::Offline`], so the message waits.
    offline: std::sync::atomic::AtomicBool,
    /// Signed in with a provider: an access token instead of the password.
    token: Option<beavermail_core::TokenSource>,
}

impl SmtpSender {
    /// Creates the sender, signing in as the account's outgoing login with `password`; the connection
    /// is opened for each message.
    pub fn new(account: &Account, password: String) -> Self {
        Self { server: account.smtp.clone(), user: account.smtp_login().to_owned(), password, offline: Default::default(), token: None }
    }

    /// Signs in with `token`'s access tokens (XOAUTH2) instead of the password.
    pub fn with_token(mut self, token: beavermail_core::TokenSource) -> Self {
        self.token = Some(token);
        self
    }

    /// Connects and signs in, sending nothing: whether the server, its TLS and the login work.
    pub async fn check(&self) -> Result<(), MailError> {
        if self.offline.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(MailError::Offline("working offline".into()));
        }
        self.transport().await?.test_connection().await.map(|_| ()).map_err(smtp_error)
    }

    /// The transport for one message, signed in with the password or, for a provider, a fresh token.
    async fn transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>, MailError> {
        let (secret, mechanisms) = match &self.token {
            // Asked off the async threads: getting a token may ask the provider for a new one.
            Some(token) => {
                let token = token.clone();
                (tokio::task::spawn_blocking(move || token()).await.map_err(backend)??, Some(vec![Mechanism::Xoauth2]))
            }
            None => (self.password.clone(), None),
        };
        let Server { host, port, tls } = &self.server;
        let builder = match tls {
            Tls::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)),
            Tls::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
            Tls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        }
        .map_err(backend)?;
        let builder = builder.port(*port).credentials(Credentials::new(self.user.clone(), secret)).hello_name(local_client(host, *port).await);
        Ok(match mechanisms {
            Some(mechanisms) => builder.authentication(mechanisms),
            None => builder,
        }
        .build())
    }
}

/// How the client names itself in EHLO: the address this computer reaches the server from, as an
/// address literal (RFC 5321 4.1.4), as desktop mail programs do. The library's default, `[127.0.0.1]`,
/// is a loopback address the server never sees the client at, which spam filters hold against the
/// message; the machine's own name would tell its hostname to every server on the way.
async fn local_client(host: &str, port: u16) -> lettre::transport::smtp::extension::ClientId {
    use lettre::transport::smtp::extension::ClientId;
    let Ok(Some(server)) = tokio::net::lookup_host((host, port)).await.map(|mut found| found.next()) else { return ClientId::default() };
    // A UDP socket "connected" to the server sends nothing; it only picks the local address of the route.
    let unspecified: std::net::SocketAddr = if server.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
    let local = std::net::UdpSocket::bind(unspecified).and_then(|socket| socket.connect(server).and_then(|()| socket.local_addr()));
    match local.map(|a| a.ip()) {
        Ok(std::net::IpAddr::V4(ip)) if !ip.is_loopback() && !ip.is_unspecified() => ClientId::Ipv4(ip),
        Ok(std::net::IpAddr::V6(ip)) if !ip.is_loopback() && !ip.is_unspecified() => ClientId::Ipv6(ip),
        _ => ClientId::default(),
    }
}

fn smtp_error(e: lettre::transport::smtp::Error) -> MailError {
    match e {
        e if e.status().is_some_and(|code| code.to_string() == "535") => MailError::Auth(e.to_string()),
        // What is left once the server's answers, TLS and the message itself are ruled out: no connection.
        e if !(e.is_response() || e.is_client() || e.is_transient() || e.is_permanent() || e.is_tls()) => MailError::Offline(e.to_string()),
        e => backend(e),
    }
}

fn backend(e: impl std::fmt::Display) -> MailError {
    MailError::Backend(e.to_string())
}

fn address(text: &str) -> Result<Mailbox, MailError> {
    text.trim().parse().map_err(|_| backend(format!("{:?} is not a valid email address", text.trim())))
}

/// Whether `text` is an address a message can go to ("ada@example.org", "Ada <ada@example.org>"). A
/// domain without a dot ("ada@example") is allowed by the standard but is nearly always a slip.
pub fn valid_address(text: &str) -> bool {
    text.trim().parse::<Mailbox>().is_ok_and(|m| {
        let domain = m.email.domain();
        domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
    })
}

/// A Message-ID for a message sent now, at the sender's domain: unique by the time, this process and a count.
fn new_message_id(from: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let domain = from.rsplit('@').next().map(|d| d.trim().trim_end_matches('>')).filter(|d| !d.is_empty() && !d.contains(' ')).unwrap_or("beavermail.invalid");
    format!("{nanos:x}.{:x}.{:x}@{domain}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed))
}

/// Builds the RFC 5322 message, checking every address.
#[cfg(test)]
fn build(message: &Outgoing) -> Result<Message, MailError> {
    build_with(message, &new_message_id(&message.from), std::time::SystemTime::now(), false)
}

/// As [`build`], with its Message-ID and date given, so the copy kept in Sent can be the same message.
/// `keep_bcc`: the Bcc header stays (the sender's copy); without it only the envelope carries them.
/// The message as it goes out: with the sender's key in an Autocrypt header, and encrypted when asked.
fn protect(message: &Outgoing, raw: Vec<u8>) -> Result<Vec<u8>, MailError> {
    let raw = match &message.autocrypt {
        Some(public) => beavermail_crypto::add_autocrypt(&raw, address(&message.from)?.email.as_ref(), public),
        None => raw,
    };
    match &message.seal {
        Some(seal) => beavermail_crypto::seal(&raw, seal).map_err(|e| backend(format!("could not encrypt the message: {e}"))),
        None => Ok(raw),
    }
}

fn build_with(message: &Outgoing, id: &str, date: std::time::SystemTime, keep_bcc: bool) -> Result<Message, MailError> {
    let mut builder = Message::builder().from(address(&message.from)?).subject(message.subject.clone());
    if message.to.is_empty() && message.cc.is_empty() && message.bcc.is_empty() {
        return Err(backend("add at least one recipient"));
    }
    for to in &message.to {
        builder = builder.to(address(to)?);
    }
    for cc in &message.cc {
        builder = builder.cc(address(cc)?);
    }
    for bcc in &message.bcc {
        builder = builder.bcc(address(bcc)?);
    }
    if keep_bcc {
        builder = builder.keep_bcc();
    }
    if let Some(id) = &message.in_reply_to {
        builder = builder.in_reply_to(bracketed(id));
    }
    if !message.references.is_empty() {
        let refs: Vec<String> = message.references.iter().map(|r| bracketed(r)).collect();
        builder = builder.references(refs.join(" "));
    }
    let builder = builder.date(date).message_id(Some(bracketed(id)));
    with_content(builder, message)
}

/// Sets the body: plain text, or Markdown plus its HTML, with the attachments after it; an iTIP message
/// as the plain text and its `text/calendar` part, as alternatives (RFC 6047).
fn with_content(builder: lettre::message::MessageBuilder, message: &Outgoing) -> Result<Message, MailError> {
    if let Some(calendar) = &message.calendar {
        let method = calendar.lines().find_map(|l| l.trim().strip_prefix("METHOD:")).unwrap_or("REPLY").trim().to_owned();
        let kind = ContentType::parse(&format!("text/calendar; method={method}; charset=utf-8")).map_err(backend)?;
        let parts = MultiPart::alternative().singlepart(SinglePart::plain(message.body.clone())).singlepart(SinglePart::builder().header(kind).body(calendar.clone()));
        return builder.multipart(parts).map_err(backend);
    }
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
    let markdown = keep_blank_lines(markdown);
    let events = Parser::new_ext(&markdown, Options::ENABLE_STRIKETHROUGH).filter_map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Some(Event::Text(text)),
        // A line break the writer typed stays one (a signature's lines would run together otherwise).
        Event::SoftBreak => Some(Event::HardBreak),
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

/// Markdown folds a run of blank lines into one, but a message keeps the room its writer left: each
/// blank line after the first becomes a line holding only a zero-width space, which Markdown keeps
/// and every mail program shows as an empty line. Code blocks are left as they are.
fn keep_blank_lines(markdown: &str) -> String {
    let lines: Vec<&str> = markdown.split('\n').collect();
    let (mut out, mut fenced, mut i) = (Vec::with_capacity(lines.len()), false, 0);
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
        }
        if fenced || !line.is_empty() {
            out.push(lines[i].to_owned());
            i += 1;
            continue;
        }
        let run = lines[i..].iter().take_while(|l| l.trim().is_empty()).count();
        out.push(String::new());
        // Not at the very end: trailing blank lines are just the end of the message.
        if run >= 2 && i + run < lines.len() {
            out.extend(std::iter::repeat_n("\u{200B}".to_owned(), run - 1));
            out.push(String::new());
        }
        i += run;
    }
    out.join("\n")
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
    // A draft keeps its blind copies, so they are there when it is opened again on any device.
    for bcc in message.bcc.iter().filter_map(|a| a.trim().parse::<Mailbox>().ok()) {
        builder = builder.bcc(bcc);
    }
    builder = builder.keep_bcc();
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

impl SmtpSender {
    pub fn set_offline(&self, offline: bool) {
        self.offline.store(offline, std::sync::atomic::Ordering::Relaxed);
    }
}

impl MailSender for SmtpSender {
    async fn send(&self, message: Outgoing) -> Result<Vec<u8>, MailError> {
        if self.offline.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(MailError::Offline("working offline".into()));
        }
        // What the recipients get has no Bcc header; the copy for Sent, otherwise the same, keeps it.
        let (id, date) = (new_message_id(&message.from), std::time::SystemTime::now());
        let email = build_with(&message, &id, date, false)?;
        let sent = if message.bcc.is_empty() { email.formatted() } else { build_with(&message, &id, date, true)?.formatted() };
        let (outgoing, sent) = (protect(&message, email.formatted())?, protect(&message, sent)?);
        self.transport().await?.send_raw(email.envelope(), &outgoing).await.map_err(smtp_error)?;
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_any_character_makes_an_address_mail_can_go_to() {
        for name in ["Porkbun Support Dept.", "Doe, John", "Ada (work)", "Ada: Lovelace", "info@x.org", "José \"Pepe\" Müller", "O'Brien"] {
            let item = beavermail_core::address_text(name, "someone@example.org");
            assert!(valid_address(&item), "{name:?} gave {item:?}");
        }
        assert_eq!(beavermail_core::address_text("Ada", "ada@x.org"), "Ada <ada@x.org>", "a plain name stays plain");
        assert_eq!(beavermail_core::address_text("ADA@x.org", "ada@x.org"), "ada@x.org", "the name is the address");
    }

    #[test]
    fn an_address_needs_a_domain_with_a_dot() {
        for good in ["ada@example.org", "Ada Lovelace <ada@example.org>", "\"Doe, John\" <john@mail.example.co.uk>"] {
            assert!(valid_address(good), "{good}");
        }
        for bad in ["ada@acmecorp", "ada", "ada@", "@example.org", "ada@example.", "a b@example.org"] {
            assert!(!valid_address(bad), "{bad}");
        }
    }

    #[test]
    fn blank_lines_the_writer_left_stay_but_code_is_untouched() {
        assert_eq!(keep_blank_lines("a\n\nb"), "a\n\nb", "one blank line: a new paragraph, as always");
        assert_eq!(keep_blank_lines("a\n\n\n\nb"), "a\n\n\u{200B}\n\u{200B}\n\nb");
        assert_eq!(keep_blank_lines("a\n\n\n"), "a\n", "trailing blank lines are the end of the text");
        assert_eq!(keep_blank_lines("```\nx\n\n\ny\n```"), "```\nx\n\n\ny\n```");
        assert!(markdown_html("a\n\n\n\nb").contains("\u{200B}"), "the room shows in the HTML too");
    }

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
    fn an_answer_to_an_invitation_carries_its_calendar_part_beside_the_text() {
        let mut answer = outgoing();
        answer.calendar = Some("BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n".into());
        let text = String::from_utf8(build(&answer).unwrap().formatted()).unwrap();
        assert!(text.contains("multipart/alternative") && text.contains("text/calendar; method=REPLY"), "{text}");
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
        assert!(markdown_html("Ada\nAcme").contains("Ada<br />\nAcme"), "a typed line break stays a line break");
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
