// SPDX-License-Identifier: AGPL-3.0-only

//! IMAP backend: implements [`MailSource`] over one account.

use std::fmt;
use std::sync::Arc;

use async_imap::types::{Fetch, Flag, NameAttribute};
use async_imap::{Client, Session};
use beavermail_core::{Account, MailError, MailSource, Mailbox, MessageId, MessageSummary, Role, Server, Tls};
use futures::TryStreamExt;
use mail_parser::MessageParser;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;

/// How many of the newest messages a mailbox listing fetches.
// shortcut: no paging, a mailbox shows only its latest messages; add paging with the local cache.
const LIST_LIMIT: u32 = 200;

trait Conn: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug> Conn for T {}
type Stream = Box<dyn Conn>;

#[derive(Default)]
struct State {
    session: Option<Session<Stream>>,
    /// Mailbox names from the last LIST; a [`MessageId`] holds an index into this.
    mailboxes: Vec<String>,
    /// Special-use folders from the last LIST (RFC 6154), by name.
    archive: Option<String>,
    trash: Option<String>,
}

/// What a write does to one message.
#[derive(Clone, Copy)]
enum Op {
    Seen(bool),
    Archive,
    Trash,
    /// Permanent delete; allowed only inside the trash folder.
    Erase,
}

pub struct ImapSource {
    config: Config,
    state: Mutex<State>,
}

/// What a connection needs: the account's IMAP server and its credentials.
struct Config {
    server: Server,
    user: String,
    password: String,
}

impl ImapSource {
    /// Creates the source; the connection is opened on first use.
    pub fn new(account: &Account, password: String) -> Self {
        let config = Config { server: account.imap.clone(), user: account.user.clone(), password };
        Self { config, state: Mutex::new(State::default()) }
    }
}

fn backend(e: impl fmt::Display) -> MailError {
    MailError::Backend(e.to_string())
}

async fn tls(tcp: TcpStream, host: &str) -> Result<Stream, MailError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(backend)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_owned()).map_err(backend)?;
    let stream = TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(backend)?;
    Ok(Box::new(stream))
}

async fn connect(c: &Config) -> Result<Session<Stream>, MailError> {
    let Server { host, port, tls: mode } = &c.server;
    let tcp = TcpStream::connect((host.as_str(), *port)).await.map_err(backend)?;
    let client = match mode {
        Tls::None => Client::new(Box::new(tcp) as Stream),
        Tls::Tls => Client::new(tls(tcp, host).await?),
        Tls::StartTls => {
            let mut plain = Client::new(tcp);
            plain.read_response().await.map_err(backend)?; // server greeting
            plain.run_command_and_check_ok("STARTTLS", None).await.map_err(backend)?;
            Client::new(tls(plain.into_inner(), host).await?)
        }
    };
    client.login(&c.user, &c.password).await.map_err(|(e, _)| MailError::Auth(e.to_string()))
}

impl State {
    /// The open session, connecting first if needed.
    async fn session(&mut self, config: &Config) -> Result<&mut Session<Stream>, MailError> {
        if self.session.is_none() {
            self.session = Some(connect(config).await?);
        }
        Ok(self.session.as_mut().expect("just connected"))
    }

    /// Maps an IMAP error and drops the session, so the next call reconnects.
    fn failed(&mut self, e: async_imap::error::Error) -> MailError {
        self.session = None;
        backend(e)
    }

    async fn list(&mut self, config: &Config) -> Result<Vec<Mailbox>, MailError> {
        let mut special = (None, None);
        let result = async {
            let session = self.session(config).await?;
            let mut names: Vec<String> = session
                .list(None, Some("*"))
                .await
                .map_err(backend)?
                .try_filter(|n| futures::future::ready(!n.attributes().contains(&NameAttribute::NoSelect)))
                .map_ok(|n| {
                    let attrs = n.attributes();
                    (n.name().to_owned(), attrs.contains(&NameAttribute::Archive), attrs.contains(&NameAttribute::Trash))
                })
                .try_collect::<Vec<_>>()
                .await
                .map_err(backend)?
                .into_iter()
                .map(|(name, archive, trash)| {
                    if archive {
                        special.0 = Some(name.clone());
                    }
                    if trash {
                        special.1 = Some(name.clone());
                    }
                    name
                })
                .collect();
            names.sort_by_key(|n| (n != "INBOX", n.clone()));
            let mut mailboxes = Vec::with_capacity(names.len());
            for name in &names {
                let status = session.status(name, "(UNSEEN)").await.map_err(backend)?;
                mailboxes.push(Mailbox { name: name.clone(), unread: status.unseen.unwrap_or(0), role: Role::Other });
            }
            Ok((names, mailboxes))
        }
        .await;
        match result {
            Ok((names, mut mailboxes)) => {
                // shortcut: servers without RFC 6154 flags fall back to common folder names.
                let named = |wanted: &[&str]| names.iter().find(|n| wanted.iter().any(|w| n.eq_ignore_ascii_case(w))).cloned();
                self.archive = special.0.or_else(|| named(&["Archive", "Archives"]));
                self.trash = special.1.or_else(|| named(&["Trash", "Deleted Items", "Deleted Messages"]));
                for mailbox in &mut mailboxes {
                    mailbox.role = if self.archive.as_ref() == Some(&mailbox.name) {
                        Role::Archive
                    } else if self.trash.as_ref() == Some(&mailbox.name) {
                        Role::Trash
                    } else {
                        Role::Other
                    };
                }
                self.mailboxes = names;
                Ok(mailboxes)
            }
            Err(e) => {
                self.session = None;
                Err(e)
            }
        }
    }

    async fn mailbox_name(&mut self, config: &Config, index: usize) -> Result<String, MailError> {
        if self.mailboxes.is_empty() {
            self.list(config).await?;
        }
        self.mailboxes.get(index).cloned().ok_or(MailError::NotFound)
    }
}

/// A message id packs the mailbox index and the IMAP UID.
// shortcut: ignores UIDVALIDITY; ids are only valid for the session that listed them, until the cache lands.
fn encode(mailbox: usize, uid: u32) -> MessageId {
    MessageId(((mailbox as u64) << 32) | u64::from(uid))
}

fn decode(id: MessageId) -> (usize, u32) {
    ((id.0 >> 32) as usize, id.0 as u32)
}

fn summary(mailbox: usize, fetch: &Fetch) -> Option<MessageSummary> {
    let uid = fetch.uid?;
    let message = MessageParser::default().parse_headers(fetch.header()?)?;
    let from = message
        .from()
        .and_then(|a| a.first())
        .map(|a| a.name().or(a.address()).unwrap_or_default().to_owned())
        .unwrap_or_default();
    let received = message.date().map_or_else(String::new, |d| {
        format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute)
    });
    Some(MessageSummary {
        id: encode(mailbox, uid),
        from,
        subject: message.subject().unwrap_or_default().to_owned(),
        preview: String::new(), // shortcut: needs the body; filled once bodies are cached
        received,
        unread: !fetch.flags().any(|f| f == Flag::Seen),
    })
}

impl ImapSource {
    /// Applies `op` to one message. Moves use `MOVE` where the server has it, else copy and delete.
    async fn write(&self, id: MessageId, op: Op) -> Result<(), MailError> {
        let (index, uid) = decode(id);
        let mut state = self.state.lock().await;
        let mailbox = state.mailbox_name(&self.config, index).await?;
        let target = match op {
            Op::Seen(_) => None,
            Op::Archive => Some(state.archive.clone().ok_or_else(|| backend("this account has no archive folder"))?),
            Op::Trash => Some(state.trash.clone().ok_or_else(|| backend("this account has no trash folder"))?),
            Op::Erase => None,
        };
        if matches!(op, Op::Erase) && state.trash.as_deref() != Some(mailbox.as_str()) {
            return Err(backend("only messages in Trash can be erased"));
        }
        if target.as_deref() == Some(mailbox.as_str()) {
            return Err(backend(format!("the message is already in {mailbox}")));
        }
        let session = state.session(&self.config).await?;
        let result = async {
            session.select(&mailbox).await?;
            let uid = uid.to_string();
            match (op, target) {
                (Op::Seen(seen), _) => {
                    let query = if seen { "+FLAGS.SILENT (\\Seen)" } else { "-FLAGS.SILENT (\\Seen)" };
                    session.uid_store(&uid, query).await?.try_collect::<Vec<_>>().await?;
                }
                (_, Some(dest)) => {
                    if session.uid_mv(&uid, &dest).await.is_err() {
                        session.uid_copy(&uid, &dest).await?;
                        session.uid_store(&uid, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                        session.uid_expunge(&uid).await?.try_collect::<Vec<_>>().await?;
                    }
                }
                (Op::Erase, _) => {
                    session.uid_store(&uid, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                    // shortcut: needs UIDPLUS; without it the message stays flagged \\Deleted rather than risk expunging others.
                    session.uid_expunge(&uid).await?.try_collect::<Vec<_>>().await?;
                }
                (_, None) => {}
            }
            Ok::<(), async_imap::error::Error>(())
        }
        .await;
        result.map_err(|e| state.failed(e))
    }
}

impl MailSource for ImapSource {
    async fn mailboxes(&self) -> Result<Vec<Mailbox>, MailError> {
        self.state.lock().await.list(&self.config).await
    }

    async fn messages(&self, mailbox: &str) -> Result<Vec<MessageSummary>, MailError> {
        let mut state = self.state.lock().await;
        if state.mailboxes.is_empty() {
            state.list(&self.config).await?;
        }
        let index = state.mailboxes.iter().position(|n| n == mailbox).ok_or(MailError::NotFound)?;
        let session = state.session(&self.config).await?;
        let result = async {
            let selected = session.examine(mailbox).await?; // read-only: never marks messages seen
            if selected.exists == 0 {
                return Ok(Vec::new());
            }
            let first = selected.exists.saturating_sub(LIST_LIMIT - 1).max(1);
            let fetches: Vec<Fetch> = session
                .fetch(format!("{first}:*"), "(UID FLAGS BODY.PEEK[HEADER])")
                .await?
                .try_collect()
                .await?;
            Ok(fetches)
        }
        .await;
        let fetches = result.map_err(|e| state.failed(e))?;
        let mut messages: Vec<_> = fetches.iter().filter_map(|f| summary(index, f)).collect();
        messages.sort_by_key(|m| std::cmp::Reverse(m.id.0));
        Ok(messages)
    }

    async fn body(&self, id: MessageId) -> Result<String, MailError> {
        let (index, uid) = decode(id);
        let mut state = self.state.lock().await;
        let mailbox = state.mailbox_name(&self.config, index).await?;
        let session = state.session(&self.config).await?;
        let result = async {
            session.examine(&mailbox).await?;
            let fetches: Vec<Fetch> = session.uid_fetch(uid.to_string(), "BODY.PEEK[]").await?.try_collect().await?;
            Ok(fetches)
        }
        .await;
        let fetches = result.map_err(|e| state.failed(e))?;
        let raw = fetches.iter().find_map(|f| f.body()).ok_or(MailError::NotFound)?;
        let message = MessageParser::default().parse(raw).ok_or(MailError::NotFound)?;
        Ok(message.body_text(0).map(|t| t.into_owned()).unwrap_or_default())
    }

    async fn set_unread(&self, id: MessageId, unread: bool) -> Result<(), MailError> {
        self.write(id, Op::Seen(!unread)).await
    }

    async fn archive(&self, id: MessageId) -> Result<(), MailError> {
        self.write(id, Op::Archive).await
    }

    async fn trash(&self, id: MessageId) -> Result<(), MailError> {
        self.write(id, Op::Trash).await
    }

    async fn erase(&self, id: MessageId) -> Result<(), MailError> {
        self.write(id, Op::Erase).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trips() {
        assert_eq!(decode(encode(7, 4_000_000_000)), (7, 4_000_000_000));
    }
}
