// SPDX-License-Identifier: AGPL-3.0-only

//! IMAP backend: implements [`MailSource`] over one account.

use std::fmt;
use std::sync::Arc;

use async_imap::types::{Fetch, Flag, NameAttribute};
use async_imap::{Client, Session};
use beavermail_core::{Account, MailError, MailSource, Mailbox, MessageBody, MessageId, MessageSummary, Role, Server, SyncReport, Tls};
use beavermail_store::{Header, Location, Store};
use beavermail_vault::{Vault, folder_path, message_path};
use futures::TryStreamExt;
use mail_parser::MessageParser;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;

/// Messages fetched by the first sync of a folder: the newest, so the list can fill at once.
const FIRST_CHUNK: u32 = 300;
/// How many UIDs of older history one sync call downloads.
const BACKFILL_SPAN: u32 = 1000;
/// Cached messages whose flags and existence are re-checked on every sync.
const RECONCILE_WINDOW: u32 = 1000;
/// Header fields the list and threading need; the rest of the header is not downloaded.
const HEADER_FIELDS: &str = "BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES)]";

trait Conn: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug> Conn for T {}
type Stream = Box<dyn Conn>;

#[derive(Default)]
struct State {
    session: Option<Session<Stream>>,
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
    /// Identity of the account in the archive.
    account: String,
    store: Arc<Store>,
    /// Where the downloaded messages live, encrypted.
    files: Arc<Vault>,
    state: Mutex<State>,
}

/// What a connection needs: the account's IMAP server and its credentials.
struct Config {
    server: Server,
    user: String,
    password: String,
}

impl ImapSource {
    /// Creates the source over the shared archive; the connection is opened on first use.
    pub fn new(account: &Account, password: String, store: Arc<Store>, files: Arc<Vault>) -> Self {
        let config = Config { server: account.imap.clone(), user: account.user.clone(), password };
        Self { config, account: account.key(), store, files, state: Mutex::new(State::default()) }
    }

    /// Path of a message's file in the vault.
    fn file_of(&self, account: &str, folder: &str, uid: u32) -> Result<std::path::PathBuf, MailError> {
        let validity = self.store.folder_state(account, folder)?.uidvalidity.unwrap_or(0);
        Ok(message_path(account, folder, validity, uid))
    }

    /// Deletes the files of messages that no longer exist on the server.
    fn drop_files(&self, folder: &str, uids: &[u32]) {
        for uid in uids {
            if let Ok(path) = self.file_of(&self.account, folder, *uid) {
                let _ = self.files.remove(&path); // a leftover file is harmless; the next reset removes the folder
            }
        }
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
}

/// The role an RFC 6154 special-use attribute announces.
fn role_of(attributes: &[NameAttribute<'_>]) -> Role {
    attributes
        .iter()
        .find_map(|a| match a {
            NameAttribute::Archive => Some(Role::Archive),
            NameAttribute::Drafts => Some(Role::Drafts),
            NameAttribute::Flagged => Some(Role::Starred),
            NameAttribute::Junk => Some(Role::Junk),
            NameAttribute::Sent => Some(Role::Sent),
            NameAttribute::Trash => Some(Role::Trash),
            _ => None,
        })
        .unwrap_or(Role::Other)
}

/// Marks INBOX, then gives a role by its usual name to folders the server did not flag.
// shortcut: servers without RFC 6154 flags are matched by English folder names.
fn resolve_roles(folders: &mut [(String, Role)]) {
    const BY_NAME: [(Role, &[&str]); 5] = [
        (Role::Drafts, &["Drafts"]),
        (Role::Sent, &["Sent", "Sent Items", "Sent Messages"]),
        (Role::Archive, &["Archive", "Archives"]),
        (Role::Junk, &["Junk", "Spam", "Junk Email"]),
        (Role::Trash, &["Trash", "Deleted Items", "Deleted Messages"]),
    ];
    for (name, role) in folders.iter_mut() {
        if name.eq_ignore_ascii_case("INBOX") {
            *role = Role::Inbox;
        }
    }
    for (wanted, names) in BY_NAME {
        if folders.iter().any(|(_, r)| *r == wanted) {
            continue;
        }
        if let Some((_, role)) = folders.iter_mut().find(|(n, r)| *r == Role::Other && names.iter().any(|w| n.eq_ignore_ascii_case(w))) {
            *role = wanted;
        }
    }
}

/// The Message-IDs of a header such as `References`, without angle brackets.
fn ids(value: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    match value.as_text_list() {
        Some(list) => list.iter().map(|id| id.to_string()).collect(),
        None => value.as_text().map(|id| vec![id.to_owned()]).unwrap_or_default(),
    }
}

/// A header block as the archive stores it, or `None` when the fetch has no UID or header.
fn header(fetch: &Fetch) -> Option<Header> {
    let uid = fetch.uid?;
    let raw = fetch.header().or_else(|| fetch.section(&async_imap::imap_proto::SectionPath::Full(async_imap::imap_proto::MessageSection::Header)))?;
    let message = MessageParser::default().parse_headers(raw)?;
    let sender = message.from().and_then(|a| a.first());
    let date = message.date();
    let received = date.map_or_else(String::new, |d| format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute));
    let timestamp = date.map(|d| d.to_timestamp()).or_else(|| fetch.internal_date().map(|d| d.timestamp())).unwrap_or(0);
    Some(Header {
        uid,
        from_name: sender.and_then(|a| a.name()).unwrap_or_default().to_owned(),
        from_addr: sender.and_then(|a| a.address()).unwrap_or_default().to_owned(),
        subject: message.subject().unwrap_or_default().to_owned(),
        received,
        timestamp,
        unread: !fetch.flags().any(|f| f == Flag::Seen),
        message_id: message.message_id().unwrap_or_default().to_owned(),
        in_reply_to: ids(message.in_reply_to()).join(" "),
        references: ids(message.references()).join(" "),
    })
}

/// "Name <address>" or a bare address for each entry of an address header.
fn addresses(header: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    header
        .into_iter()
        .flat_map(|h| h.iter())
        .filter_map(|a| match (a.name(), a.address()) {
            (Some(name), Some(address)) => Some(format!("{name} <{address}>")),
            (None, Some(address)) => Some(address.to_owned()),
            _ => None,
        })
        .collect()
}

impl ImapSource {
    /// The folder with `role`, listing the account's folders first if the archive has none yet.
    async fn folder_with_role(&self, role: Role) -> Result<Option<String>, MailError> {
        if self.store.folders(&self.account)?.is_empty() {
            self.mailboxes().await?;
        }
        self.store.folder_with_role(&self.account, role)
    }

    /// Applies `op` to one message on the server, then mirrors it in the archive.
    async fn write(&self, id: MessageId, op: Op) -> Result<(), MailError> {
        let at = self.store.locate(id)?.ok_or(MailError::NotFound)?;
        let target = match op {
            Op::Seen(_) | Op::Erase => None,
            Op::Archive => Some(self.folder_with_role(Role::Archive).await?.ok_or_else(|| backend("this account has no archive folder"))?),
            Op::Trash => Some(self.folder_with_role(Role::Trash).await?.ok_or_else(|| backend("this account has no trash folder"))?),
        };
        if matches!(op, Op::Erase) && self.folder_with_role(Role::Trash).await?.as_deref() != Some(at.folder.as_str()) {
            return Err(backend("only messages in Trash can be erased"));
        }
        if target.as_deref() == Some(at.folder.as_str()) {
            return Err(backend(format!("the message is already in {}", at.folder)));
        }
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            session.select(&at.folder).await?;
            let uid = at.uid.to_string();
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
        result.map_err(|e| state.failed(e))?;
        match op {
            Op::Seen(seen) => self.store.set_unread(id, !seen),
            Op::Archive | Op::Trash | Op::Erase => {
                self.drop_files(&at.folder, &[at.uid]);
                self.store.remove(id)
            }
        }
    }

    async fn fetch_headers(session: &mut Session<Stream>, set: &str, by_uid: bool) -> Result<Vec<Header>, async_imap::error::Error> {
        let query = format!("(UID FLAGS INTERNALDATE {HEADER_FIELDS})");
        let fetches: Vec<Fetch> = if by_uid {
            session.uid_fetch(set, &query).await?.try_collect().await?
        } else {
            session.fetch(set, &query).await?.try_collect().await?
        };
        Ok(fetches.iter().filter_map(header).collect())
    }

    /// The synchronisation itself; see [`MailSource::sync`].
    async fn sync_folder(&self, folder: &str) -> Result<SyncReport, MailError> {
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = self.sync_with(session, folder).await;
        match result {
            Ok(report) => Ok(report),
            Err(SyncError::Archive(e)) => Err(e),
            Err(SyncError::Imap(e)) => Err(state.failed(e)),
        }
    }

    async fn sync_with(&self, session: &mut Session<Stream>, folder: &str) -> Result<SyncReport, SyncError> {
        let (account, store) = (self.account.as_str(), &*self.store);
        let selected = session.examine(folder).await?;
        let validity = selected.uid_validity.unwrap_or(0);
        let mut cached = store.folder_state(account, folder)?;
        if cached.uidvalidity != Some(validity) {
            store.reset_folder(account, folder, validity)?;
            self.files.remove_dir(&folder_path(account, folder))?;
            cached.floor_uid = None;
        }
        if selected.exists == 0 {
            let all: Vec<u32> = store.flags_since(account, folder, 0)?.into_iter().map(|(u, _)| u).collect();
            self.drop_files(folder, &all);
            store.remove_uids(account, folder, &all)?;
            return Ok(SyncReport::default());
        }

        // New messages. The first sync takes the newest few by sequence number.
        let range = store.uid_range(account, folder)?;
        let fresh = match range {
            Some((_, highest, _)) => Self::fetch_headers(session, &format!("{}:*", highest + 1), true).await?.into_iter().filter(|h| h.uid > highest).collect(),
            None => Self::fetch_headers(session, &format!("{}:*", selected.exists.saturating_sub(FIRST_CHUNK - 1).max(1)), false).await?,
        };
        if cached.floor_uid.is_none() {
            let lowest = fresh.iter().map(|h| h.uid).min().or(range.map(|(lo, _, _)| lo));
            // Everything fetched when the folder is small; otherwise history below the lowest UID is missing.
            let floor = if selected.exists <= FIRST_CHUNK { 1 } else { lowest.unwrap_or(1) };
            store.set_floor(account, folder, floor)?;
            cached.floor_uid = Some(floor);
        }
        store.add_headers(account, folder, &fresh)?;

        // Flags and deletions among the newest cached messages.
        if let Some(from) = store.nth_newest_uid(account, folder, RECONCILE_WINDOW)?.or(store.uid_range(account, folder)?.map(|(lo, _, _)| lo)) {
            let fetches: Vec<Fetch> = session.uid_fetch(format!("{from}:*"), "(UID FLAGS)").await?.try_collect().await?;
            let server: std::collections::HashMap<u32, bool> = fetches.iter().filter_map(|f| Some((f.uid?, !f.flags().any(|fl| fl == Flag::Seen)))).collect();
            let local = store.flags_since(account, folder, from)?;
            let gone: Vec<u32> = local.iter().filter(|(uid, _)| !server.contains_key(uid)).map(|(uid, _)| *uid).collect();
            let changed: Vec<(u32, bool)> = local.iter().filter_map(|(uid, unread)| server.get(uid).filter(|s| *s != unread).map(|s| (*uid, *s))).collect();
            self.drop_files(folder, &gone);
            store.remove_uids(account, folder, &gone)?;
            store.set_unread_by_uid(account, folder, &changed)?;
        }

        // One more chunk of older history.
        let mut floor = cached.floor_uid.unwrap_or(1);
        for _ in 0..20 {
            if floor <= 1 {
                break;
            }
            let start = floor.saturating_sub(BACKFILL_SPAN).max(1);
            let older = Self::fetch_headers(session, &format!("{start}:{}", floor - 1), true).await?;
            store.add_headers(account, folder, &older)?;
            floor = start;
            store.set_floor(account, folder, floor)?;
            if !older.is_empty() {
                break;
            }
        }
        let have = store.uid_range(account, folder)?.map_or(0, |(_, _, count)| count);
        Ok(SyncReport { remaining: if floor <= 1 { 0 } else { selected.exists.saturating_sub(have) } })
    }
}

/// Why a sync step failed: the connection (drop it) or the local archive (keep it).
enum SyncError {
    Imap(async_imap::error::Error),
    Archive(MailError),
}

impl From<async_imap::error::Error> for SyncError {
    fn from(e: async_imap::error::Error) -> Self {
        Self::Imap(e)
    }
}

impl From<MailError> for SyncError {
    fn from(e: MailError) -> Self {
        Self::Archive(e)
    }
}

/// The text, recipients and threading headers of a complete message.
fn parse_body(raw: &[u8]) -> Result<MessageBody, MailError> {
    let message = MessageParser::default().parse(raw).ok_or(MailError::NotFound)?;
    Ok(MessageBody {
        text: message.body_text(0).map(|t| t.into_owned()).unwrap_or_default(),
        to: addresses(message.to()),
        cc: addresses(message.cc()),
        reply_to: addresses(message.reply_to()),
        message_id: message.message_id().unwrap_or_default().to_owned(),
        html: message.html_bodies().find_map(|part| match &part.body {
            mail_parser::PartType::Html(html) => Some(html.to_string()),
            _ => None,
        }),
        references: ids(message.references()),
    })
}

impl ImapSource {
    /// The complete message from the server.
    async fn download(&self, at: &Location) -> Result<Vec<u8>, MailError> {
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            session.examine(&at.folder).await?;
            let fetches: Vec<Fetch> = session.uid_fetch(at.uid.to_string(), "BODY.PEEK[]").await?.try_collect().await?;
            Ok(fetches)
        }
        .await;
        let fetches = result.map_err(|e| state.failed(e))?;
        fetches.iter().find_map(|f| f.body().map(<[u8]>::to_vec)).ok_or(MailError::NotFound)
    }
}

impl MailSource for ImapSource {
    async fn cached_mailboxes(&self) -> Result<Vec<Mailbox>, MailError> {
        Ok(self.store.folders(&self.account)?.into_iter().map(|f| Mailbox { name: f.name, unread: f.unread, role: f.role }).collect())
    }

    async fn mailboxes(&self) -> Result<Vec<Mailbox>, MailError> {
        let mut state = self.state.lock().await;
        let result = async {
            let session = state.session(&self.config).await?;
            let mut folders: Vec<(String, Role)> = session
                .list(None, Some("*"))
                .await
                .map_err(backend)?
                .try_filter(|n| futures::future::ready(!n.attributes().contains(&NameAttribute::NoSelect)))
                .map_ok(|n| (n.name().to_owned(), role_of(n.attributes())))
                .try_collect::<Vec<_>>()
                .await
                .map_err(backend)?;
            folders.sort_by_key(|(n, _)| (n != "INBOX", n.clone()));
            resolve_roles(&mut folders);
            let mut mailboxes = Vec::with_capacity(folders.len());
            for (name, role) in &folders {
                let status = session.status(name, "(UNSEEN)").await.map_err(backend)?;
                mailboxes.push(Mailbox { name: name.clone(), unread: status.unseen.unwrap_or(0), role: *role });
            }
            Ok((folders, mailboxes))
        }
        .await;
        match result {
            Ok((folders, mailboxes)) => {
                self.store.set_folders(&self.account, &folders)?;
                Ok(mailboxes)
            }
            Err(e) => {
                state.session = None;
                Err(e)
            }
        }
    }

    async fn messages(&self, mailbox: &str) -> Result<Vec<MessageSummary>, MailError> {
        self.store.messages(&self.account, mailbox)
    }

    async fn sync(&self, mailbox: &str) -> Result<SyncReport, MailError> {
        self.sync_folder(mailbox).await
    }

    async fn body(&self, id: MessageId) -> Result<MessageBody, MailError> {
        let at = self.store.locate(id)?.ok_or(MailError::NotFound)?;
        let path = self.file_of(&at.account, &at.folder, at.uid)?;
        // A file that cannot be read (damaged, written under another key, or the vault is locked) is
        // downloaded again.
        if let Some(raw) = self.files.read(&path).ok().flatten() {
            return parse_body(&raw);
        }
        let raw = self.download(&at).await?;
        let body = parse_body(&raw)?;
        // Not being able to keep the file (a locked vault) must not stop the message from opening.
        if self.files.write(&path, &raw).is_ok() {
            self.store.set_preview(id, &body.text)?;
        }
        Ok(body)
    }

    async fn save_sent(&self, raw: Vec<u8>) -> Result<(), MailError> {
        let sent = self.folder_with_role(Role::Sent).await?.ok_or_else(|| backend("this account has no Sent folder"))?;
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = session.append(&sent, Some("(\\Seen)"), None, &raw).await;
        result.map_err(|e| state.failed(e))
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
    fn roles_come_from_flags_then_names() {
        let mut folders = vec![
            ("INBOX".to_owned(), Role::Other),
            ("Sent Items".to_owned(), Role::Other),
            ("Old".to_owned(), Role::Archive), // flagged by the server
            ("Archive".to_owned(), Role::Other), // same name, but the flag wins
            ("Spam".to_owned(), Role::Other),
        ];
        resolve_roles(&mut folders);
        let roles: Vec<Role> = folders.into_iter().map(|(_, r)| r).collect();
        assert_eq!(roles, [Role::Inbox, Role::Sent, Role::Archive, Role::Other, Role::Junk]);
    }

}
